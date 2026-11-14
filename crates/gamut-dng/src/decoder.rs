//! The DNG decoder: parse a DNG back to its raw image, colour profile, and version.
//!
//! This reverses [`crate::encoder`]. It walks the IFD tree (IFD 0 plus the raw sub-IFD reached
//! through `SubIFDs`), decompresses and unpacks the sensor samples, and reconstructs the
//! [`RawImage`] and [`CameraProfile`]. As stated in the crate docs, demosaicing and colour
//! rendering are out of scope — the decoder returns the sensor samples, not a viewable image.
//!
//! This module owns the public decode surface, the consumption tracking that feeds the verbatim
//! channels, embedded metadata, and the raw IFD's own tags (levels, masked areas, opcode lists,
//! gain maps). Each other concern is a submodule:
//!
//! - `walk` collects every directory of the IFD tree and selects the raw IFD among them;
//! - `sub_image` decodes each non-raw image IFD (previews, masks, depth maps);
//! - `image_data` decodes the strip or tile data the raw image and sub-images share;
//! - `profile_tags` reads IFD 0's colour calibration and version, and the typed value readers.

mod image_data;
mod profile_tags;
mod sub_image;
mod walk;

use std::cell::RefCell;
use std::slice;

use gamut_core::{Dimensions, Error, Result};
use gamut_ifd::tags::STANDARD_POINTER_TAGS;
use gamut_ifd::{
    ByteOrder, C2PA_MANIFEST_STORE, C2paExclusions, Ifd, SubtreeScope, Value, Variant,
    c2pa_exclusions, read, read_ifd_at, resolve_subtree,
};

use self::image_data::{chunk_grid, decode_image_data, grid_chunks};
pub(crate) use self::profile_tags::f64_vec;
use self::profile_tags::{bytes_value, decode_profile, ratio, read_version, unsigned_f64s};
use self::sub_image::{decode_depth_info, decode_sub_image};
use self::walk::{select_raw_ifd, walk_ifds};
use crate::color_profile::{ColorProfileInfo, NoiseProfile, TagSource};
use crate::gain_map::ProfileGainTableMap;
use crate::levels::{RawLevels, checked_black_repeat};
use crate::metadata::DngMetadata;
use crate::opcode::OpcodeList;
use crate::profile::CameraProfile;
use crate::raw::RawImage;
use crate::subimage::{DepthInfo, SubImage};
use crate::values::{Compression, PhotometricInterpretation, dng_compression, dng_photometric};
use crate::{compression, tags};

/// One IFD entry preserved verbatim: the tag number and its fully typed [`Value`].
///
/// This is how the decoder represents a field it does not model — private maker tags, DNG
/// features without a typed surface yet — so such fields are not silently dropped (issue #109's
/// decode contract). The value is `gamut-ifd`'s typed enum, not opaque bytes.
///
/// The contract holds over six channels — [`DecodedDng::ifd0_extra`],
/// [`DecodedDng::raw_extra`], [`SubImage::extra_tags`], [`DecodedDng::trailing_extra`],
/// [`DecodedDng::interior_extra`] and [`DecodedDng::sub_ifd_extra`] — which together reach every
/// directory the decoder walks, with one residue, documented on
/// [`trailing_extra`](DecodedDng::trailing_extra): a **duplicated tag** keeps only the last
/// entry, because the eager [`Ifd`] this is built from is last-wins.
/// [`deconstruct`](crate::deconstruct()) still accounts for the bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct RawTag {
    /// The TIFF/DNG tag number.
    pub tag: u16,
    /// The entry's decoded value.
    pub value: Value,
}

/// The unmodelled fields of one interior main-chain page, as reported by
/// [`DecodedDng::interior_extra`].
///
/// `#[non_exhaustive]`: the decoder builds these; a caller reads the fields.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct PageExtra {
    /// The page's 0-based position in the main IFD chain (IFD 0 is page 0, the directory IFD 0's
    /// next-IFD pointer names is page 1, and so on).
    pub page: usize,
    /// Every field of that page the pipeline did not consume, in tag order, values verbatim.
    pub tags: Vec<RawTag>,
}

/// The unmodelled fields of one image-less directory reached through `SubIFDs` (330), as
/// reported by [`DecodedDng::sub_ifd_extra`].
///
/// The directory is identified by its position in the IFD tree: the main-chain
/// [`page`](Self::page) whose `SubIFDs` tree it hangs from, then the [`path`](Self::path) of
/// `SubIFDs` array positions leading down to it.
///
/// `#[non_exhaustive]`: the decoder builds these; a caller reads the fields.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct SubIfdExtra {
    /// The 0-based main-chain page the directory descends from, numbered as
    /// [`PageExtra::page`] is (IFD 0 is page 0).
    pub page: usize,
    /// The 0-based position of the directory in each successive `SubIFDs` array, outermost
    /// first: `[1]` is the second entry of the page's own `SubIFDs`, `[1, 0]` the first entry
    /// of *that* directory's `SubIFDs`. Never empty — a directory with an empty path is a
    /// main-chain page, which [`PageExtra`] and the other channels cover. Positions count every
    /// array entry, including one the walk skipped (an offset already visited or unreadable),
    /// so a path always indexes the array as the file stores it.
    pub path: Vec<usize>,
    /// Every field of that directory the pipeline did not consume, in tag order, values
    /// verbatim.
    pub tags: Vec<RawTag>,
}

/// An [`Ifd`] wrapper that records every tag the decode pipeline consumes, so the tags *not*
/// consumed can be surfaced verbatim ([`TrackedIfd::remaining`]) — correct by construction: a
/// helper that stops reading a tag makes that tag reappear in the extras, which golden tests
/// pin. Interior mutability keeps the read API `&self` like [`Ifd`]'s.
struct TrackedIfd<'a> {
    ifd: &'a Ifd,
    seen: RefCell<Vec<u16>>,
}

impl<'a> TrackedIfd<'a> {
    fn new(ifd: &'a Ifd) -> Self {
        Self {
            ifd,
            seen: RefCell::new(Vec::new()),
        }
    }

    /// Marks `tag` consumed (whether or not it is present).
    fn touch(&self, tag: u16) {
        let mut seen = self.seen.borrow_mut();
        if !seen.contains(&tag) {
            seen.push(tag);
        }
    }

    /// Un-marks `tag`, so a value that was read but then *rejected* (e.g. an invalid
    /// `MaskSubArea`, which the spec says to ignore) still surfaces in the extras.
    fn untouch(&self, tag: u16) {
        self.seen.borrow_mut().retain(|&t| t != tag);
    }

    fn get(&self, tag: u16) -> Option<&Value> {
        self.touch(tag);
        self.ifd.get(tag)
    }

    fn get_u32(&self, tag: u16) -> Option<u32> {
        self.touch(tag);
        self.ifd.get_u32(tag)
    }

    fn get_u32_vec(&self, tag: u16) -> Option<Vec<u32>> {
        self.touch(tag);
        self.ifd.get_u32_vec(tag)
    }

    fn get_u64_vec(&self, tag: u16) -> Option<Vec<u64>> {
        self.touch(tag);
        self.ifd.get_u64_vec(tag)
    }

    /// Every field the pipeline did not consume, in tag order, values verbatim.
    fn remaining(&self) -> Vec<RawTag> {
        let seen = self.seen.borrow();
        self.ifd
            .fields()
            .iter()
            .filter(|f| !seen.contains(&f.tag))
            .map(|f| RawTag {
                tag: f.tag,
                value: f.value.clone(),
            })
            .collect()
    }
}

/// Reading a tag through a projection consumes it; rejecting a malformed one puts it back, so it
/// still reaches the extras.
impl TagSource for TrackedIfd<'_> {
    fn value(&self, tag: u16) -> Option<&Value> {
        self.get(tag)
    }

    fn reject(&self, tag: u16) {
        self.untouch(tag);
    }
}

/// A decoded DNG: the raw sensor image, the camera colour profile, and the declared DNG version.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DecodedDng {
    /// The raw sensor image (CFA mosaic or linear), with its photometry and levels.
    pub raw: RawImage,
    /// The camera colour profile reconstructed from IFD 0, when the file carries one.
    ///
    /// `None` for a DNG with no colour calibration to reconstruct — a monochrome camera has no
    /// colour to calibrate, and such files legitimately omit `ColorMatrix1`,
    /// `CalibrationIlluminant1` and `AsShotNeutral` entirely (a Leica M Monochrom writes a DNG
    /// 1.0.0.0 file carrying only `UniqueCameraModel`). The raw image still decodes; nothing is
    /// invented to fill the gap.
    pub profile: Option<CameraProfile>,
    /// IFD 0's remaining camera-profile colour tags — the hue/saturation/value tables, the tone
    /// curve, the profile exposure offset, the third calibration set and the reduction matrices —
    /// when the file carries any of them.
    ///
    /// This is the rendering half of the colour model: [`profile`](Self::profile) carries the
    /// calibration a raw processor needs to reach XYZ, this carries what the profile then asks it
    /// to do with the result.
    pub color_profile: Option<ColorProfileInfo>,
    /// The raw IFD's `NoiseProfile` (51041) — the sensor's noise model — falling back to IFD 0
    /// for a file that stores it there.
    pub noise_profile: Option<NoiseProfile>,
    /// The `DNGVersion` the file declares, as its four dotted version octets in order — e.g. DNG
    /// 1.7.1.0 is `[1, 7, 1, 0]`. Kept as four bytes (not a packed `u32`) so each component reads
    /// directly and byte order never enters into it.
    pub dng_version: [u8; 4],
    /// Embedded metadata (EXIF sub-IFD + XMP/IPTC/ICC blocks), reconstructed from IFD 0.
    ///
    /// The `ExifIFD` arrives whole, as a directory —
    /// every entry of the directory, not a chosen subset — so nothing in it needs a separate
    /// verbatim escape hatch. Its standard pointer fields (a camera's `InteroperabilityIFD`)
    /// arrive resolved, as sub-IFD groups [`DngEncoder`](crate::DngEncoder) writes back; one whose
    /// target cannot be read is dropped rather than handed back as a stale file offset.
    pub metadata: DngMetadata,
    /// The raw IFD's `ProfileGainTableMap` (52525), if present.
    pub gain_table_map: Option<ProfileGainTableMap>,
    /// IFD 0's `ProfileGainTableMap2` (52544), if present. When both maps are present, this one
    /// supersedes [`gain_table_map`](Self::gain_table_map) for rendering (DNG 1.7.1 p. 88).
    pub gain_table_map2: Option<ProfileGainTableMap>,
    /// Every non-raw image IFD in the file — previews, transparency/semantic masks, depth maps,
    /// enhanced images — decoded where the scheme is in scope, verbatim chunks otherwise.
    pub sub_images: Vec<SubImage>,
    /// IFD 0's depth-map description tags, when any is present.
    pub depth_info: Option<DepthInfo>,
    /// The `DNGBackwardVersion` the file declares, if present.
    pub backward_version: Option<[u8; 4]>,
    /// The stored `NewRawImageDigest` (51111), if present — as written, not recomputed. Compare
    /// against [`RawImage::new_raw_image_digest`] to verify raw-data integrity.
    pub new_raw_image_digest: Option<[u8; 16]>,
    /// Every IFD 0 field the pipeline does not model, verbatim — proprietary maker tags
    /// included — in tag order.
    ///
    /// This is one of six verbatim channels that together carry the unmodelled fields of every
    /// directory the decoder walks: this one, [`raw_extra`](Self::raw_extra),
    /// [`SubImage::extra_tags`] per image directory,
    /// [`trailing_extra`](Self::trailing_extra) for the last main-chain directory when it is
    /// none of those, [`interior_extra`](Self::interior_extra) for every earlier main-chain
    /// page that is none of those either, and [`sub_ifd_extra`](Self::sub_ifd_extra) for every
    /// directory reached through `SubIFDs` that is none of those — so every walked directory
    /// reaches exactly one of them. Nothing is silently dropped; the one residue (a duplicated
    /// tag keeps its last entry) is documented on [`trailing_extra`](Self::trailing_extra) and
    /// in `STATUS.md`. `deconstruct` remains the byte-accounting *diagnostic* view of the same
    /// principle.
    pub ifd0_extra: Vec<RawTag>,
    /// Every unmodelled field of the raw IFD, verbatim. Empty when the raw image lives in IFD 0
    /// itself (its extras are then in [`ifd0_extra`](Self::ifd0_extra)).
    pub raw_extra: Vec<RawTag>,
    /// Every field of the **last directory of the main IFD chain**, verbatim, when that
    /// directory is not surfaced anywhere else — i.e. when it is not IFD 0, not the raw IFD and
    /// carries no image of its own, so it becomes no [`SubImage`].
    ///
    /// C2PA 2.4 §A.3.6 is what makes such a directory ordinary rather than exotic: it permits a
    /// manifest store to be "the only entity within a new IFD following the existing one", and a
    /// directory holding just that entry has no pixels to make it a sub-image. Whatever it
    /// carries reaches the caller here — including a tag-52545 field this decoder declined to
    /// read as a store (wrong type, too short, or duplicated).
    ///
    /// Empty for every file this crate writes, which puts the entry in IFD 0. The main-chain
    /// pages *before* this one that are surfaced nowhere else arrive in
    /// [`interior_extra`](Self::interior_extra) instead.
    ///
    /// # The residue of the "nothing is dropped" contract
    ///
    /// **A duplicated tag keeps the last entry.** Every typed channel is built on the eager
    /// [`Ifd`], which is last-wins, so where a directory carries two entries under one tag only
    /// the second's value arrives. Such a file is malformed for a manifest store (§A.3.6 allows
    /// one per asset). [`deconstruct`](crate::deconstruct()) accounts for the bytes.
    ///
    /// Every walked directory reaches a channel: main-chain pages this field and
    /// [`interior_extra`](Self::interior_extra), image-less `SubIFDs` children
    /// [`sub_ifd_extra`](Self::sub_ifd_extra).
    pub trailing_extra: Vec<RawTag>,
    /// Every field of each **interior main-chain page** — a top-level directory that is neither
    /// IFD 0 nor the last of the chain — that is surfaced nowhere else: not the raw IFD and
    /// carrying no image of its own, so it becomes no [`SubImage`]. One [`PageExtra`] per such
    /// page, in chain order, keyed by its position so a caller can tell which page each field
    /// came from.
    ///
    /// The last main-chain directory is [`trailing_extra`](Self::trailing_extra)'s, not this
    /// field's: that one is the C2PA 2.4 §A.3.6 placement, and keeps its own channel. A page
    /// listed here is listed even when every one of its fields was consumed (its `tags` are then
    /// empty), so the list says which pages the typed surface did not otherwise reach.
    ///
    /// Empty for every file this crate writes, which has one main-chain directory.
    pub interior_extra: Vec<PageExtra>,
    /// Every field of each directory reached through a `SubIFDs` (330) pointer — at any depth,
    /// below any main-chain page — that is surfaced nowhere else: not the raw IFD and carrying
    /// no image of its own (no strip or tile offsets, missing or zero dimensions, or chunks too
    /// malformed to carry), so it becomes no [`SubImage`]. One [`SubIfdExtra`] per such
    /// directory, in walk order (each page, then its `SubIFDs` tree depth-first), keyed by its
    /// position in the IFD tree so a caller can tell which directory each field came from.
    ///
    /// A directory listed here is listed even when every one of its fields was consumed (its
    /// `tags` are then empty), as [`interior_extra`](Self::interior_extra) does for pages. A
    /// directory the walk did not follow — past the nesting cap, already visited, or unreadable
    /// — is not walked, so it is in no channel; `deconstruct` diagnoses those. Separately, the
    /// decoder treats a `SubIFDs` entry naming a main-chain page (IFD 0 itself, or a later page)
    /// as already visited: that page is reported once, in its own page channel, never here as
    /// well. That skip is the decoder's alone and is not diagnosed: `deconstruct`'s walk seeds no
    /// page offsets, so it follows such an entry as an ordinary child and raises no
    /// skipped-entry or cycle finding for the entry itself (only the re-walked page's own
    /// `SubIFDs`, met again, can draw one).
    ///
    /// Empty for every file this crate writes, whose `SubIFDs` children all carry images.
    pub sub_ifd_extra: Vec<SubIfdExtra>,
    /// The C2PA manifest store the file carries in the last IFD of its main chain (C2PA 2.4
    /// §A.3.6), as the `UNDEFINED` bytes of its `C2PA` tag
    /// ([`gamut_ifd::C2PA_MANIFEST_STORE`], 52545), verbatim — the file's byte order does not
    /// apply to them. `Some` exactly when [`c2pa_exclusions`](Self::c2pa_exclusions) is.
    ///
    /// A store is bound to the exact file it was signed over (§18.5), so one copied into another
    /// file through [`DngEncoder::with_c2pa`](crate::DngEncoder::with_c2pa) is invalid by
    /// construction.
    pub c2pa: Option<Vec<u8>>,
    /// Where the C2PA manifest store in [`c2pa`](Self::c2pa) sits in the file:
    /// the two ranges a `c2pa.hash.data` binding excludes (C2PA 2.4 §18.5.5), located by
    /// [`gamut_ifd::c2pa_exclusions`] in the last IFD of the main chain (§A.3.6). `Some` exactly
    /// when the store is; a `C2PA` tag of a type other than `UNDEFINED` is not a store and stays
    /// in the verbatim channel of the directory that holds it — [`ifd0_extra`](Self::ifd0_extra)
    /// when that is IFD 0, [`trailing_extra`](Self::trailing_extra) when it is a trailing
    /// directory surfaced nowhere else (or [`raw_extra`](Self::raw_extra) /
    /// [`SubImage::extra_tags`] when the last main-chain directory is the raw IFD or a
    /// sub-image).
    pub c2pa_exclusions: Option<C2paExclusions>,
}

/// The verdict of [`DngDecoder::verify_new_raw_image_digest`].
///
/// `#[non_exhaustive]`: further verdicts (e.g. a transparency-mask-inclusive digest) may be added
/// without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DigestCheck {
    /// The file carries no `NewRawImageDigest` tag, so there is nothing to verify. Common: no
    /// Apple ProRAW or Leica file in the wild writes one.
    Absent,
    /// The recomputed digest equals the stored one — the raw data is intact.
    Match,
    /// The digests differ: the raw data does not match what the writer recorded.
    Mismatch {
        /// The digest the file stores.
        stored: [u8; 16],
        /// The digest recomputed from the file's raw image.
        computed: [u8; 16],
    },
}

/// Decoder for DNG (Adobe Digital Negative) raw images.
#[derive(Debug, Clone, Default)]
pub struct DngDecoder {
    _private: (),
}

impl DngDecoder {
    /// Creates a decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Verifies a file's stored `NewRawImageDigest` (51111) against the digest recomputed from its
    /// raw image, choosing the rule the file's own storage demands.
    ///
    /// Two rules exist and picking the wrong one produces a spurious mismatch, which is why this
    /// is a verb on the decoder rather than a value the caller compares by hand:
    ///
    /// - **Lossless storage** (uncompressed, Deflate, lossless JPEG) digests the *samples*, via
    ///   [`RawImage::new_raw_image_digest`].
    /// - **Lossy-compressed storage** (JPEG XL, lossy JPEG) digests the *compressed chunks* in
    ///   offset order — the SDK's `dng_lossy_compressed_image::FindDigest`. Because this rule
    ///   never decodes pixels, it verifies files whose compression this crate cannot yet decode
    ///   at all: a lossy-JPEG DNG's integrity is checkable even though its image is not.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`decode`](Self::decode) for a malformed container, and — for
    /// lossless storage only — whatever decoding the raw image reports.
    pub fn verify_new_raw_image_digest(&self, data: &[u8]) -> Result<DigestCheck> {
        let file = read(data)?;
        let (ifds, _) = walk_ifds(&file, data);
        let raw_index = select_raw_ifd(&ifds)?;
        let tracked: Vec<TrackedIfd> = ifds.iter().map(TrackedIfd::new).collect();
        let Some(stored) = bytes_value(tracked[0].get(tags::NEW_RAW_IMAGE_DIGEST))
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
        else {
            return Ok(DigestCheck::Absent);
        };

        let raw_ifd = &tracked[raw_index];
        let compression = dng_compression(raw_ifd.get_u32(tags::COMPRESSION).unwrap_or(1) as u16)
            .ok_or_else(|| {
            Error::unsupported(env!("CARGO_PKG_NAME"), "DNG: unknown compression")
        })?;
        let computed = if compression::is_lossy(compression) {
            let grid = chunk_grid(
                raw_ifd,
                raw_ifd.get_u32(tags::IMAGE_WIDTH).unwrap_or(0) as usize,
                raw_ifd.get_u32(tags::IMAGE_LENGTH).unwrap_or(0) as usize,
            )?;
            let chunks = grid_chunks(raw_ifd, data, &grid)?;
            let owned: Vec<Vec<u8>> = chunks.iter().map(|c| c.to_vec()).collect();
            crate::digest::lossy_compressed_digest(&owned)
        } else {
            decode_raw_image(raw_ifd, data, file.order)?.new_raw_image_digest()
        };

        Ok(if computed == stored {
            DigestCheck::Match
        } else {
            DigestCheck::Mismatch { stored, computed }
        })
    }

    /// Decodes `data` (a DNG file) into its raw image, profile, and version.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// container is malformed or a required tag is missing, or an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a compression scheme
    /// or photometry not yet decodable.
    pub fn decode(&self, data: &[u8]) -> Result<DecodedDng> {
        // `read` guarantees at least one IFD, so index 0 below is IFD 0.
        let file = read(data)?;
        let order = file.order;
        let variant = file.variant;

        let (ifds, positions) = walk_ifds(&file, data);
        // The main-chain pages are the walked directories with an empty `SubIFDs` path.
        let mains: Vec<usize> = positions
            .iter()
            .enumerate()
            .filter(|(_, (_, path))| path.is_empty())
            .map(|(i, _)| i)
            .collect();
        // `read` guarantees a non-empty chain, so the fallback (IFD 0, no earlier pages) is
        // only the shape that guarantee already gives a one-page file.
        let (&last_main, earlier_mains) = mains.split_last().unwrap_or((&0, &[]));
        let raw_index = select_raw_ifd(&ifds)?;
        // One consumption tracker per IFD; `walk_ifds` pushes IFD 0 first, so index 0 is IFD 0
        // (and `raw_index == 0` means the raw image lives in IFD 0 itself). Tags the walk/select
        // phase consumed on plain IFDs are marked up front.
        let tracked: Vec<TrackedIfd> = ifds.iter().map(TrackedIfd::new).collect();
        for t in &tracked {
            t.touch(tags::SUB_IFDS);
            t.touch(tags::NEW_SUBFILE_TYPE);
            t.touch(tags::PHOTOMETRIC_INTERPRETATION);
        }
        let ifd0 = &tracked[0];
        let raw_ifd = &tracked[raw_index];

        let raw = decode_raw_image(raw_ifd, data, order)?;
        let profile = decode_profile(ifd0)?;
        let color_profile = crate::color_profile::project(ifd0);
        // The spec stores `NoiseProfile` in the raw (or enhanced) IFD; some writers put it in
        // IFD 0 instead, so fall back there when the raw IFD is a distinct directory.
        let mut noise_profile = crate::color_profile::project_noise(raw_ifd);
        if noise_profile.is_none() && raw_index != 0 {
            noise_profile = crate::color_profile::project_noise(ifd0);
        }
        let dng_version = read_version(ifd0)?;
        let backward_version = bytes_value(ifd0.get(tags::DNG_BACKWARD_VERSION)).map(|b| {
            let mut v = [0u8; 4];
            for (slot, byte) in v.iter_mut().zip(b) {
                *slot = byte;
            }
            v
        });
        let new_raw_image_digest = bytes_value(ifd0.get(tags::NEW_RAW_IMAGE_DIGEST))
            .and_then(|b| <[u8; 16]>::try_from(b).ok());
        // The C2PA store lives in the last IFD of the main chain (C2PA 2.4 §A.3.6) — IFD 0 for
        // every file this crate writes, a trailing directory for the other form §A.3.6 allows.
        // The ranges are located *first* and the bytes are then taken only if that succeeded, so
        // the two surfaces cannot disagree about whether the file has a store: one rule, applied
        // once. (Reading the bytes independently is what let a duplicated entry report bytes
        // from the eager `Ifd`, which keeps the last duplicate, while the ranges reported none.)
        let c2pa_exclusions = c2pa_exclusions(data)?;
        let (metadata, c2pa) = decode_metadata(
            ifd0,
            &tracked[last_main],
            c2pa_exclusions.is_some(),
            data,
            order,
            variant,
        );
        let gain_table_map = decode_gain_map(raw_ifd, tags::PROFILE_GAIN_TABLE_MAP, order)?;
        let gain_table_map2 = decode_gain_map(ifd0, tags::PROFILE_GAIN_TABLE_MAP2, order)?;
        let depth_info = decode_depth_info(ifd0);
        // Sub-images last, so an IFD-0 preview's extras reflect every root-level consumer above.
        let mut sub_images = Vec::new();
        let mut sub_indices = Vec::new();
        for (i, t) in tracked.iter().enumerate() {
            if i == raw_index {
                continue;
            }
            if let Some(sub) = decode_sub_image(t, data, order) {
                sub_images.push(sub);
                sub_indices.push(i);
            }
        }
        // Extras are computed only after every consumer has run. IFD 0's unmodelled tags go to
        // `ifd0_extra` alone (even when IFD 0 doubles as the raw IFD or a preview sub-image).
        for (sub, &i) in sub_images.iter_mut().zip(&sub_indices) {
            if i != 0 {
                sub.extra_tags = tracked[i].remaining();
            }
        }
        let ifd0_extra = tracked[0].remaining();
        let raw_extra = if raw_index == 0 {
            Vec::new()
        } else {
            tracked[raw_index].remaining()
        };
        // The last directory of the main chain is where C2PA 2.4 §A.3.6 puts a manifest store,
        // and such a directory legitimately carries no image at all — so it is not IFD 0, not
        // the raw IFD, and not a sub-image, and without this its fields would reach no surface
        // at all. That would break this decoder's standing promise that nothing in the file is
        // silently dropped, for exactly the input the store's own placement rule invites.
        //
        // Stated as one membership test over the directories already surfaced rather than as a
        // chain of `||`s: the disjuncts never disagree on a real file (a single-main-IFD DNG
        // makes the first true, a trailing store directory makes all three false), so the
        // operators between them decided nothing and no test could pin them.
        let mut surfaced = vec![0, raw_index];
        surfaced.extend_from_slice(&sub_indices);
        let trailing_extra = if surfaced.contains(&last_main) {
            Vec::new()
        } else {
            tracked[last_main].remaining()
        };
        // Every *earlier* main-chain page the channels above did not reach — a page with no
        // image of its own, between IFD 0 and the last directory (issue #525). Reported per
        // page, keyed by its chain position, so a caller can tell which page a field came from.
        // IFD 0 is always in `surfaced`, so page 0 never appears here.
        let interior_extra = earlier_mains
            .iter()
            .enumerate()
            .filter(|(_, i)| !surfaced.contains(i))
            .map(|(page, &i)| PageExtra {
                page,
                tags: tracked[i].remaining(),
            })
            .collect();
        // Every directory below a page, reached through `SubIFDs`, that the channels above did
        // not reach — one with no image of its own that is not the raw IFD (issue #667).
        // Reported per directory, keyed by its tree position.
        let sub_ifd_extra = positions
            .iter()
            .enumerate()
            .filter(|(i, (_, path))| !path.is_empty() && !surfaced.contains(i))
            .map(|(i, (page, path))| SubIfdExtra {
                page: *page,
                path: path.clone(),
                tags: tracked[i].remaining(),
            })
            .collect();

        Ok(DecodedDng {
            raw,
            profile,
            color_profile,
            noise_profile,
            dng_version,
            metadata,
            gain_table_map,
            gain_table_map2,
            sub_images,
            depth_info,
            backward_version,
            new_raw_image_digest,
            ifd0_extra,
            raw_extra,
            trailing_extra,
            interior_extra,
            sub_ifd_extra,
            c2pa,
            c2pa_exclusions,
        })
    }
}

/// Reconstructs embedded metadata from IFD 0 — the XMP/IPTC/ICC blocks and the EXIF sub-IFD —
/// and the C2PA manifest store from `store_ifd`, the last IFD of the main chain.
///
/// The `ExifIFD` is handed over whole, as a directory: every entry it holds survives, so no field
/// of it is "unmodelled". The one exception is a standard pointer field (`InteroperabilityIFD`
/// in a camera file): it arrives as the sub-IFD group it points at, or — when its target cannot
/// be read — not at all, never as a raw offset into this file ([`resolve_exif_pointers`]). The
/// DNG's own IFD 0 is *not* returned as EXIF — [`DecodedDng`] already carries those fields,
/// typed or as [`ifd0_extra`](DecodedDng::ifd0_extra).
///
/// The store is taken as the `UNDEFINED` bytes C2PA 2.4 §A.3.6 mandates, verbatim — the file's
/// byte order does not apply to them — and only when `located` says
/// [`gamut_ifd::c2pa_exclusions`] recognised a store in this file. That flag is the *whole*
/// admission rule (type, length, uniqueness, placement), so this cannot disagree with
/// [`DecodedDng::c2pa_exclusions`]: bytes and ranges are `Some` together or neither is.
///
/// A tag-52545 field that is not an admissible store — wrong type, shorter than a JUMBF box
/// header, or one of several in a directory §A.3.6 allows only one store in — is put back for
/// the extras rather than returned, so it still reaches the caller and decode → encode still
/// works: every store this hands back is one the encoder will accept.
fn decode_metadata(
    ifd0: &TrackedIfd,
    store_ifd: &TrackedIfd,
    located: bool,
    data: &[u8],
    order: ByteOrder,
    variant: Variant,
) -> (DngMetadata, Option<Vec<u8>>) {
    let c2pa = match store_ifd.get(C2PA_MANIFEST_STORE) {
        Some(Value::Undefined(store)) if located => Some(store.clone()),
        Some(_) => {
            store_ifd.untouch(C2PA_MANIFEST_STORE);
            None
        }
        None => None,
    };
    let exif = ifd0
        .get_u32(tags::EXIF_IFD)
        .and_then(|offset| read_ifd_at(data, u64::from(offset), order, variant).ok())
        .map(|mut exif_ifd| {
            resolve_exif_pointers(&mut exif_ifd, |offset| {
                read_ifd_at(data, offset, order, variant)
            });
            crate::metadata::exif_model(exif_ifd, order)
        });
    let metadata = DngMetadata {
        exif,
        xmp: bytes_value(ifd0.get(tags::XMP)),
        iptc: bytes_value(ifd0.get(tags::IPTC_NAA)),
        icc: bytes_value(ifd0.get(tags::ICC_PROFILE)),
    };
    (metadata, c2pa)
}

/// The deepest level below the Exif directory [`resolve_exif_pointers`] follows: **one**, the
/// `InteroperabilityIFD` EXIF 2.3 §4.6.3 puts inside it. Nothing conformant nests a directory
/// below that, so a pointer found there is out of spec — the same bound `gamut-tiff`'s metadata
/// read applies (two levels below IFD 0, which is one below `ExifIFD`).
const MAX_EXIF_POINTER_DEPTH: usize = 1;

/// Replaces each standard pointer field of the Exif directory (`SubIFDs`, `ExifIFD`, `GPSInfo`,
/// `InteroperabilityIFD` — [`STANDARD_POINTER_TAGS`]) with a sub-IFD group holding the
/// directories it points at, which `fetch` parses by offset.
///
/// A pointer left as a field is a raw absolute offset into *this* file: it means nothing in a
/// directory handed to a caller, and [`DngEncoder`](crate::DngEncoder) refuses to write one, so a
/// decode → encode round trip of a camera DNG's metadata would fail on it. A group is what the
/// encoder writes back, re-pointed at the copy it lays out, and what
/// [`DngRewrite::open`](crate::DngRewrite::open) reads back. The tags followed are the ones that
/// reader resolves at every node; inside each followed directory the same four are followed,
/// [`MAX_EXIF_POINTER_DEPTH`] deep.
///
/// **An unreadable target is dropped, not fatal.** Each tag resolves on its own: when its walk
/// fails — a target outside the file or not a directory, a cycle, or a pointer nested deeper than
/// the bound — that one field is removed and the rest of the Exif directory is kept. That is the
/// same leniency this decoder already applies to an unreadable `ExifIFD` itself and to an
/// unreadable `SubIFDs` child ([`walk_ifds`]): metadata never fails a raw decode. Keeping the
/// field instead would hand back the offset the encoder refuses, and failing would lose every
/// other Exif entry over one broken pointer. A field under one of these tags whose type is not an
/// offset (e.g. a `SHORT`) is not a pointer and stays as it is.
fn resolve_exif_pointers(exif: &mut Ifd, mut fetch: impl FnMut(u64) -> Result<Ifd>) {
    for tag in STANDARD_POINTER_TAGS {
        let Some(value) = exif.get(*tag) else {
            continue;
        };
        // Resolved in a one-field holder standing in for the Exif directory (depth 0), so a
        // failure leaves the real directory untouched apart from the removal below.
        let mut holder = Ifd::new();
        holder.set(*tag, value.clone());
        let scope = SubtreeScope::new(slice::from_ref(tag), STANDARD_POINTER_TAGS)
            .with_max_depth(MAX_EXIF_POINTER_DEPTH);
        if resolve_subtree(&mut holder, scope, &mut fetch).is_err() {
            exif.remove(*tag);
            continue;
        }
        for group in holder.sub_ifds() {
            exif.set_sub_ifd(group.tag, group.ifds.clone());
        }
    }
}

/// Reconstructs the [`RawImage`] from a raw IFD and the file's strip data.
fn decode_raw_image(ifd: &TrackedIfd, data: &[u8], order: ByteOrder) -> Result<RawImage> {
    let width = ifd.get_u32(tags::IMAGE_WIDTH).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: raw IFD missing ImageWidth")
    })?;
    let height = ifd.get_u32(tags::IMAGE_LENGTH).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: raw IFD missing ImageLength")
    })?;
    let spp = ifd.get_u32(tags::SAMPLES_PER_PIXEL).unwrap_or(1);
    let bits = ifd
        .get_u32_vec(tags::BITS_PER_SAMPLE)
        .and_then(|v| v.first().copied())
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: raw IFD missing BitsPerSample")
        })? as u16;
    // JPEG XL data decodes to full-range 16-bit whatever precision the codestream stores (the
    // reference SDK's semantics; Apple ProRAW declares BitsPerSample 10 with WhiteLevel 65535).
    // The reconstructed image therefore carries 16 significant bits.
    let bits = if ifd.get_u32(tags::COMPRESSION) == Some(u32::from(u16::from(Compression::JpegXl)))
    {
        16
    } else {
        bits
    };
    let photometric = ifd
        .get_u32(tags::PHOTOMETRIC_INTERPRETATION)
        .and_then(|c| u16::try_from(c).ok())
        .and_then(dng_photometric)
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: raw IFD missing PhotometricInterpretation",
            )
        })?;

    let samples = decode_image_data(ifd, data, order, width, height, spp as usize, bits)?;

    let dims = Dimensions::new(width, height)?;
    let mut raw = match photometric {
        PhotometricInterpretation::Cfa => {
            let dim = ifd
                .get_u32_vec(tags::CFA_REPEAT_PATTERN_DIM)
                .filter(|v| v.len() == 2)
                .ok_or_else(|| {
                    Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "DNG: CFA missing CFARepeatPatternDim",
                    )
                })?;
            let pattern = bytes_value(ifd.get(tags::CFA_PATTERN)).ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: CFA missing CFAPattern")
            })?;
            let repeat = (dim[0] as u16, dim[1] as u16);
            let mut raw = RawImage::new_cfa(dims, bits, repeat, pattern, samples)?;
            if let Some(colors) = bytes_value(ifd.get(tags::CFA_PLANE_COLOR)) {
                raw = raw.with_cfa_plane_color(colors);
            }
            if let Some(layout) = ifd
                .get_u32(tags::CFA_LAYOUT)
                .and_then(|c| u16::try_from(c).ok())
                .and_then(|c| crate::values::CfaLayout::try_from(c).ok())
            {
                raw = raw.with_cfa_layout(layout);
            }
            raw
        }
        PhotometricInterpretation::LinearRaw => {
            RawImage::new_linear_raw(dims, bits, spp as u16, samples)?
        }
        _ => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "DNG: photometry is not a raw image",
            ));
        }
    };

    let active_area = ifd
        .get_u32_vec(tags::ACTIVE_AREA)
        .filter(|v| v.len() == 4)
        .map(|v| [v[0], v[1], v[2], v[3]]);
    if let Some(area) = active_area {
        raw = raw.with_active_area(area);
    }
    if let (Some(origin), Some(size)) = (
        ifd.get_u32_vec(tags::DEFAULT_CROP_ORIGIN)
            .filter(|v| v.len() == 2),
        ifd.get_u32_vec(tags::DEFAULT_CROP_SIZE)
            .filter(|v| v.len() == 2),
    ) {
        raw = raw.with_default_crop([origin[0], origin[1]], [size[0], size[1]]);
    }
    raw = raw.with_levels(decode_levels(ifd, spp as u16, bits, dims, active_area)?)?;
    if let Some(areas) = decode_masked_areas(ifd)? {
        raw = raw.with_masked_areas(areas);
    }
    if let Some(list) = decode_opcode_list(ifd, tags::OPCODE_LIST1)? {
        raw = raw.with_opcode_list1(list);
    }
    if let Some(list) = decode_opcode_list(ifd, tags::OPCODE_LIST2)? {
        raw = raw.with_opcode_list2(list);
    }
    if let Some(list) = decode_opcode_list(ifd, tags::OPCODE_LIST3)? {
        raw = raw.with_opcode_list3(list);
    }
    Ok(raw)
}

/// Reads a `ProfileGainTableMap`/`ProfileGainTableMap2` tag (UNDEFINED bytes, file byte order)
/// into its typed model; the tag number selects the version's layout. A malformed map is an
/// error, not silently dropped.
fn decode_gain_map(
    ifd: &TrackedIfd,
    tag: u16,
    order: ByteOrder,
) -> Result<Option<ProfileGainTableMap>> {
    let Some(value) = ifd.get(tag) else {
        return Ok(None);
    };
    let bytes = value.as_bytes().ok_or_else(|| {
        Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: gain-table maps must be UNDEFINED byte data",
        )
    })?;
    let map = if tag == tags::PROFILE_GAIN_TABLE_MAP {
        ProfileGainTableMap::parse_v1(bytes, order)?
    } else {
        ProfileGainTableMap::parse_v2(bytes, order)?
    };
    Ok(Some(map))
}

/// Reads an `OpcodeList1/2/3` tag (UNDEFINED bytes) into a typed [`OpcodeList`]. The container
/// is big-endian regardless of the file's byte order (DNG 1.7.1 p. 105); a malformed container
/// is an error, not silently dropped.
fn decode_opcode_list(ifd: &TrackedIfd, tag: u16) -> Result<Option<OpcodeList>> {
    let Some(value) = ifd.get(tag) else {
        return Ok(None);
    };
    let bytes = value.as_bytes().ok_or_else(|| {
        Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: opcode lists must be UNDEFINED byte data",
        )
    })?;
    Ok(Some(OpcodeList::parse(bytes)?))
}

/// Reads the level family — `BlackLevelRepeatDim`/`BlackLevel` (+ the `DeltaH`/`DeltaV`
/// refinements) and the per-plane `WhiteLevel` — from a raw IFD (DNG 1.7.1 pp. 27–29).
///
/// A single-value `BlackLevel`/`WhiteLevel` broadcasts to every cell/plane (common writer
/// shorthand, and what pre-pattern gamut-dng emitted); any other count mismatch is an error.
/// Delta counts must match the active area (defaulting to the full image), mirroring the DNG SDK.
fn decode_levels(
    ifd: &TrackedIfd,
    spp: u16,
    bits: u16,
    dims: Dimensions,
    active_area: Option<[u32; 4]>,
) -> Result<RawLevels> {
    // Bounded before `cells` sizes anything below: the two SHORTs are all that backs the
    // pattern when `BlackLevel` is absent or broadcast (#950).
    let repeat = match ifd.get_u32_vec(tags::BLACK_LEVEL_REPEAT_DIM) {
        Some(v) if v.len() == 2 => checked_black_repeat(v[0], v[1])?,
        Some(_) => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: BlackLevelRepeatDim needs two values (rows, cols)",
            ));
        }
        None => (1, 1),
    };

    let cells = usize::from(repeat.0) * usize::from(repeat.1) * usize::from(spp);
    let black = match ifd.get(tags::BLACK_LEVEL) {
        None => vec![0.0; cells],
        Some(value) => {
            let v = unsigned_f64s(value).ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: BlackLevel must be SHORT, LONG, or RATIONAL",
                )
            })?;
            if v.len() == cells {
                v
            } else if v.len() == 1 {
                vec![v[0]; cells]
            } else {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: BlackLevel count must be repeat rows * cols * samples per pixel",
                ));
            }
        }
    };

    let white = match ifd.get_u32_vec(tags::WHITE_LEVEL) {
        None => vec![f64::from((1u32 << bits) - 1); usize::from(spp)],
        Some(v) if v.len() == usize::from(spp) => v.into_iter().map(f64::from).collect(),
        Some(v) if v.len() == 1 => vec![f64::from(v[0]); usize::from(spp)],
        Some(_) => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: WhiteLevel needs one value per sample plane",
            ));
        }
    };

    let mut levels = RawLevels::new(spp, repeat, black, white)?;

    if let Some(value) = ifd.get(tags::LINEARIZATION_TABLE) {
        let Value::Short(table) = value else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: LinearizationTable must be SHORT",
            ));
        };
        if table.is_empty() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: LinearizationTable must not be empty",
            ));
        }
        levels = levels.with_linearization_table(table.clone());
    }

    let (aa_width, aa_height) = active_area_size(dims, active_area);
    if let Some(deltas) = decode_deltas(ifd, tags::BLACK_LEVEL_DELTA_H, aa_width, "column")? {
        levels = levels.with_black_delta_h(deltas);
    }
    if let Some(deltas) = decode_deltas(ifd, tags::BLACK_LEVEL_DELTA_V, aa_height, "row")? {
        levels = levels.with_black_delta_v(deltas);
    }
    Ok(levels)
}

/// Reads a `BlackLevelDeltaH`/`BlackLevelDeltaV` tag, requiring `expected` values (one per
/// active-area column/row — the SDK enforces the same).
fn decode_deltas(
    ifd: &TrackedIfd,
    tag: u16,
    expected: usize,
    axis: &'static str,
) -> Result<Option<Vec<f64>>> {
    let Some(value) = ifd.get(tag) else {
        return Ok(None);
    };
    let deltas: Vec<f64> = value
        .as_srationals()
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: black-level deltas must be SRATIONAL",
            )
        })?
        .iter()
        .map(|&(n, d)| ratio(f64::from(n), f64::from(d)))
        .collect();
    if deltas.len() != expected {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            match axis {
                "column" => "DNG: BlackLevelDeltaH needs one value per active-area column",
                _ => "DNG: BlackLevelDeltaV needs one value per active-area row",
            },
        ));
    }
    Ok(Some(deltas))
}

/// The active-area `(width, height)`, defaulting to the full image when the tag is absent.
fn active_area_size(dims: Dimensions, active_area: Option<[u32; 4]>) -> (usize, usize) {
    match active_area {
        Some([top, left, bottom, right]) => (
            right.saturating_sub(left) as usize,
            bottom.saturating_sub(top) as usize,
        ),
        None => (dims.width() as usize, dims.height() as usize),
    }
}

/// Reads `MaskedAreas` as `[top, left, bottom, right]` rectangles (count must be a positive
/// multiple of four, DNG 1.7.1 p. 44).
fn decode_masked_areas(ifd: &TrackedIfd) -> Result<Option<Vec<[u32; 4]>>> {
    let Some(flat) = ifd.get_u32_vec(tags::MASKED_AREAS) else {
        return Ok(None);
    };
    if flat.is_empty() || flat.len() % 4 != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: MaskedAreas count must be a positive multiple of four",
        ));
    }
    Ok(Some(
        flat.as_chunks::<4>()
            .0
            .iter()
            .map(|r| [r[0], r[1], r[2], r[3]])
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use gamut_ifd::{IfdReader, TiffFile};

    use super::*;

    /// A directory carrying one `SHORT` field, `marker`, to recognise it by.
    fn marked(marker: u16) -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(1, Value::Short(vec![marker]));
        ifd
    }

    /// Runs [`resolve_exif_pointers`] over `exif` against a file holding exactly `directories`,
    /// each at its offset; any other offset is unreadable.
    fn resolved(mut exif: Ifd, directories: &[(u64, Ifd)]) -> Ifd {
        resolve_exif_pointers(&mut exif, |offset| {
            directories
                .iter()
                .find(|(at, _)| *at == offset)
                .map(|(_, ifd)| ifd.clone())
                .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "unreadable"))
        });
        exif
    }

    #[test]
    fn resolve_exif_pointers_turns_each_standard_pointer_into_its_group() {
        for &tag in STANDARD_POINTER_TAGS {
            let mut exif = marked(7);
            exif.set(tag, Value::Long(vec![100]));
            let got = resolved(exif, &[(100, marked(9))]);
            let mut want = marked(7);
            want.set_sub_ifd(tag, vec![marked(9)]);
            assert_eq!(got, want, "tag {tag}");
        }
    }

    #[test]
    fn resolve_exif_pointers_drops_a_pointer_whose_target_is_unreadable() {
        // Interop resolves while GPSInfo's target is missing: only the broken field goes, and
        // every other entry, the resolved group included, is kept.
        let mut exif = marked(7);
        exif.set(
            gamut_ifd::tags::INTEROPERABILITY_IFD,
            Value::Long(vec![100]),
        );
        exif.set(tags::GPS_INFO, Value::Long(vec![0xFFFF_FFF0]));
        let got = resolved(exif, &[(100, marked(9))]);
        let mut want = marked(7);
        want.set_sub_ifd(gamut_ifd::tags::INTEROPERABILITY_IFD, vec![marked(9)]);
        assert_eq!(got, want);
    }

    #[test]
    fn resolve_exif_pointers_leaves_a_non_offset_value_under_a_pointer_tag() {
        let mut exif = marked(7);
        exif.set(
            gamut_ifd::tags::INTEROPERABILITY_IFD,
            Value::Short(vec![100]),
        );
        assert_eq!(resolved(exif.clone(), &[(100, marked(9))]), exif);
    }

    #[test]
    fn resolve_exif_pointers_drops_a_pointer_nested_below_the_interop_directory() {
        // A pointer inside the Interop directory is one level past EXIF 2.3 §4.6.3, so the
        // InteroperabilityIFD it hangs from is dropped whole rather than handed back half-resolved.
        let mut interop = marked(9);
        interop.set(tags::GPS_INFO, Value::Long(vec![200]));
        let mut exif = marked(7);
        exif.set(
            gamut_ifd::tags::INTEROPERABILITY_IFD,
            Value::Long(vec![100]),
        );
        assert_eq!(
            resolved(exif, &[(100, interop), (200, marked(11))]),
            marked(7)
        );
    }

    #[test]
    fn decode_deltas_distinguishes_axes_and_requires_the_active_area_length() {
        for (tag, axis, message) in [
            (
                tags::BLACK_LEVEL_DELTA_H,
                "column",
                "DNG: BlackLevelDeltaH needs one value per active-area column",
            ),
            (
                tags::BLACK_LEVEL_DELTA_V,
                "row",
                "DNG: BlackLevelDeltaV needs one value per active-area row",
            ),
        ] {
            let mut ifd = Ifd::new();
            ifd.set(tag, Value::SRational(vec![(1, 2)]));
            let error = decode_deltas(&TrackedIfd::new(&ifd), tag, 2, axis).unwrap_err();
            assert_eq!(error.static_message(), Some(message));
        }
    }

    #[test]
    fn decode_masked_areas_requires_complete_nonempty_rectangles() {
        for values in [vec![], vec![0, 0, 1]] {
            let mut ifd = Ifd::new();
            ifd.set(tags::MASKED_AREAS, Value::Long(values));
            assert_eq!(
                decode_masked_areas(&TrackedIfd::new(&ifd))
                    .unwrap_err()
                    .static_message(),
                Some("DNG: MaskedAreas count must be a positive multiple of four")
            );
        }

        let mut ifd = Ifd::new();
        ifd.set(tags::MASKED_AREAS, Value::Long(vec![0, 1, 2, 3]));
        assert_eq!(
            decode_masked_areas(&TrackedIfd::new(&ifd)).unwrap(),
            Some(vec![[0, 1, 2, 3]])
        );
    }

    #[test]
    fn decode_raw_image_rejects_lossless_jpeg_sample_count_mismatch() {
        // A 4x2, single-component lossless-JPEG strip (8 samples)...
        let samples: Vec<u16> = (0..8).map(|i| i as u16).collect();
        let jpeg = gamut_jpeg::lossless::encode(&samples, 4, 2, 1, 12).expect("encode");
        // ...described by an IFD that claims a 2x2 image (4 samples). Per spec only the *total
        // sample count* must match the strip — the stream's internal width/components are free
        // (real CFA writers halve the width and double the components) — so this fails on the
        // count, not on any width/components equality.
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![2]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
        ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![12]));
        ifd.set(tags::COMPRESSION, Value::Short(vec![7])); // lossless JPEG
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803])); // CFA
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        ifd.set(
            tags::STRIP_BYTE_COUNTS,
            Value::Long(vec![jpeg.len() as u32]),
        );

        let err =
            decode_raw_image(&TrackedIfd::new(&ifd), &jpeg, ByteOrder::LittleEndian).unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("sample count"))),
            "expected a sample-count mismatch error, got {err:?}"
        );
    }

    /// A malformed lossless-JPEG strip fails with the coder's own error, unchanged: the SOF3 coder
    /// is `gamut-jpeg`'s (issue #1083), so that is the error's origin.
    #[test]
    fn a_malformed_lossless_jpeg_strip_reports_the_coders_error() {
        let strip = [0u8; 4]; // no SOI
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![1]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
        ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![16]));
        ifd.set(tags::COMPRESSION, Value::Short(vec![7]));
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![4]));
        let err =
            decode_raw_image(&TrackedIfd::new(&ifd), &strip, ByteOrder::LittleEndian).unwrap_err();
        assert_eq!(err.static_message(), Some("JPEG: missing SOI marker"));
        assert_eq!(err.origin(), Some("gamut-jpeg"));
    }

    /// A reshaped lossless-JPEG stream — half width, doubled components, same total sample
    /// count — decodes, per the spec's total-count rule (a width/components equality would
    /// wrongly reject it).
    #[test]
    fn decode_raw_image_accepts_reshaped_lossless_jpeg_stream() {
        // 8 samples encoded as a 2-wide, 2-component, 2-row stream for a 4x2 one-plane image.
        let samples: Vec<u16> = (0..8).map(|i| i as u16).collect();
        let jpeg = gamut_jpeg::lossless::encode(&samples, 2, 2, 2, 12).expect("encode");
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![4]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![2]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
        ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![12]));
        ifd.set(tags::COMPRESSION, Value::Short(vec![7]));
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
        ifd.set(tags::CFA_REPEAT_PATTERN_DIM, Value::Short(vec![2, 2]));
        ifd.set(tags::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2]));
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        ifd.set(
            tags::STRIP_BYTE_COUNTS,
            Value::Long(vec![jpeg.len() as u32]),
        );

        let raw = decode_raw_image(&TrackedIfd::new(&ifd), &jpeg, ByteOrder::LittleEndian)
            .expect("decode");
        assert_eq!(raw.samples(), &samples[..]);
    }

    /// A zlib bomb — a tiny Deflate strip that inflates to megabytes — is rejected against the
    /// strip's own geometry rather than decompressed. The cap comes from the IFD's dimensions, so
    /// the decoder never allocates on the attacker's say-so.
    #[test]
    fn decode_raw_image_rejects_a_deflate_strip_that_inflates_past_its_geometry() {
        // 2x2 CFA at 16 bits is an 8-byte strip; this one inflates to 4 MiB from ~4 KiB.
        let bomb = compression::compress(Compression::Deflate, &vec![0u8; 4 << 20]).expect("bomb");
        assert!(bomb.len() < 8192, "the bomb must be small on disk");
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![2]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
        ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![16]));
        ifd.set(tags::COMPRESSION, Value::Short(vec![8])); // Deflate
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803])); // CFA
        ifd.set(tags::CFA_REPEAT_PATTERN_DIM, Value::Short(vec![2, 2]));
        ifd.set(tags::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2]));
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        ifd.set(
            tags::STRIP_BYTE_COUNTS,
            Value::Long(vec![bomb.len() as u32]),
        );

        let err =
            decode_raw_image(&TrackedIfd::new(&ifd), &bomb, ByteOrder::LittleEndian).unwrap_err();
        assert_eq!(
            err.static_message(),
            Some("zlib stream inflates past the expected size")
        );
    }

    /// An image-less directory reached through `SubIFDs` reaches `sub_ifd_extra`, keyed by its
    /// tree position (issue #667) — and only such a directory: the raw `SubIFDs` child stays in
    /// `raw_extra`, and an image-less main-chain page stays in `trailing_extra`, so no field is
    /// reported twice and no page is mistaken for a sub-IFD.
    #[test]
    fn an_image_less_sub_ifd_is_surfaced_per_directory() {
        let software = |text: &str| RawTag {
            tag: 305,
            value: Value::Ascii(text.to_owned()),
        };
        let with_software = |text: &str| {
            let mut ifd = Ifd::new();
            ifd.set(305, Value::Ascii(text.to_owned()));
            ifd
        };
        // A 1x1 8-bit LinearRaw whose one strip byte is the file's own first byte, so the
        // fixture needs no offset fix-up after writing.
        let mut raw = with_software("raw");
        raw.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        raw.set(tags::IMAGE_LENGTH, Value::Short(vec![1]));
        raw.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8]));
        raw.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![34892]));
        raw.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        raw.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![1]));
        let mut empty = with_software("child");
        empty.set_sub_ifd(tags::SUB_IFDS, vec![with_software("grandchild")]);
        let mut ifd0 = Ifd::new();
        ifd0.set(tags::DNG_VERSION, Value::Byte(vec![1, 7, 1, 0]));
        ifd0.set_sub_ifd(tags::SUB_IFDS, vec![raw, empty]);
        let bytes = gamut_ifd::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd0, with_software("page")],
        })
        .expect("write");

        let decoded = DngDecoder::new().decode(&bytes).expect("decode");
        assert_eq!(
            decoded.sub_ifd_extra,
            [
                SubIfdExtra {
                    page: 0,
                    path: vec![1],
                    tags: vec![software("child")],
                },
                SubIfdExtra {
                    page: 0,
                    path: vec![1, 0],
                    tags: vec![software("grandchild")],
                },
            ]
        );
        assert_eq!(decoded.raw_extra, [software("raw")]);
        assert_eq!(decoded.trailing_extra, [software("page")]);
        assert!(decoded.sub_images.is_empty(), "{:?}", decoded.sub_images);
    }

    /// A two-page DNG whose IFD 0 `SubIFDs` array is `[raw, IFD 0, page 1]`: a 1x1 LinearRaw
    /// child (its one strip byte is the file's own first byte), then an entry naming IFD 0
    /// itself and one naming the second main-chain page (issue #814). Every directory carries a
    /// `Software` (305) field naming it. Shared with the walk's own tests.
    pub(super) fn sub_ifds_naming_main_chain_pages() -> Vec<u8> {
        let with_software = |text: &str| {
            let mut ifd = Ifd::new();
            ifd.set(305, Value::Ascii(text.to_owned()));
            ifd
        };
        let mut raw = with_software("raw");
        raw.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        raw.set(tags::IMAGE_LENGTH, Value::Short(vec![1]));
        raw.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8]));
        raw.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![34892]));
        raw.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        raw.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![1]));
        let mut ifd0 = with_software("ifd0");
        ifd0.set(tags::DNG_VERSION, Value::Byte(vec![1, 7, 1, 0]));
        // Placeholder children, re-pointed at the pages once the layout is known.
        ifd0.set_sub_ifd(
            tags::SUB_IFDS,
            vec![raw, with_software("stale"), with_software("stale")],
        );
        let mut bytes = gamut_ifd::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd0, with_software("page")],
        })
        .expect("write");
        let pages: Vec<u32> = IfdReader::open(&bytes[..])
            .expect("header")
            .ifds()
            .map(|raw| u32::try_from(raw.expect("page").offset).expect("classic offset"))
            .collect();
        let file = read(&bytes).expect("read");
        let offsets = file.ifds[0].get_u32_vec(tags::SUB_IFDS).expect("offsets");
        let array: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        let at = bytes
            .windows(array.len())
            .position(|w| w == array)
            .expect("the SubIFDs array is stored out of line");
        bytes[at + 4..at + 8].copy_from_slice(&pages[0].to_le_bytes());
        bytes[at + 8..at + 12].copy_from_slice(&pages[1].to_le_bytes());
        bytes
    }

    /// The page a `SubIFDs` entry names is reported once, in its page channel — never in
    /// `sub_ifd_extra` as well, which the per-directory promise of the extras forbids.
    #[test]
    fn a_sub_ifd_entry_naming_a_page_reports_the_page_once() {
        let software = |text: &str| RawTag {
            tag: 305,
            value: Value::Ascii(text.to_owned()),
        };
        let decoded = DngDecoder::new()
            .decode(&sub_ifds_naming_main_chain_pages())
            .expect("decode");
        assert_eq!(decoded.sub_ifd_extra, []);
        assert_eq!(decoded.ifd0_extra, [software("ifd0")]);
        assert_eq!(decoded.raw_extra, [software("raw")]);
        assert_eq!(decoded.trailing_extra, [software("page")]);
    }

    #[test]
    fn decode_raw_image_rejects_non_unsigned_sample_formats() {
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![2]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
        ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![16]));
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![8]));

        // Float data is a distinct, named rejection (a real, deferred DNG feature)...
        ifd.set(tags::SAMPLE_FORMAT, Value::Short(vec![3]));
        let err =
            decode_raw_image(&TrackedIfd::new(&ifd), &[0; 8], ByteOrder::LittleEndian).unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::Unsupported && error.static_message().is_some_and(|message| message.contains("floating-point"))),
            "expected a floating-point rejection, got {err:?}"
        );
        // ...signed and unrecognised formats fail generically...
        ifd.set(tags::SAMPLE_FORMAT, Value::Short(vec![2]));
        assert!(
            decode_raw_image(&TrackedIfd::new(&ifd), &[0; 8], ByteOrder::LittleEndian).is_err()
        );
        ifd.set(tags::SAMPLE_FORMAT, Value::Short(vec![9]));
        assert!(
            decode_raw_image(&TrackedIfd::new(&ifd), &[0; 8], ByteOrder::LittleEndian).is_err()
        );
        // ...and the explicit unsigned default decodes.
        ifd.set(tags::SAMPLE_FORMAT, Value::Short(vec![1]));
        ifd.set(tags::CFA_REPEAT_PATTERN_DIM, Value::Short(vec![2, 2]));
        ifd.set(tags::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2]));
        assert!(decode_raw_image(&TrackedIfd::new(&ifd), &[0; 8], ByteOrder::LittleEndian).is_ok());
    }

    /// End to end: a DNG whose raw strip is a *predictor-4* SOF3 stream — a shape gamut's
    /// own encoder never writes — decodes to identical samples through gamut's DNG decoder
    /// and the Adobe SDK's negative reader.
    ///
    /// The strip is the one `gamut-jpeg`'s lossless test-stream builder writes for these 8×6
    /// 12-bit samples under predictor 4 (one table: 15 four-bit and 2 five-bit codes over the
    /// categories in order); it is spelled out here because that builder is private to
    /// `gamut-jpeg`, which owns the coder and differentially tests every predictor there.
    #[test]
    fn predictor4_strip_dng_decodes_identically_to_adobe() {
        use gamut_ifd::{ByteOrder, Ifd, TiffFile, Value, Variant, write};

        use crate::tags;

        let (w, h) = (8usize, 6usize);
        let samples: Vec<u16> = (0..(w * h) as u32)
            .map(|i| (i.wrapping_mul(40503).wrapping_add(17) % 4096) as u16)
            .collect();
        #[rustfmt::skip]
        let strip: Vec<u8> = vec![
            255, 216, 255, 195, 0, 11, 12, 0, 6, 0, 8, 1, 1, 17, 0, 255, 196, 0, 36, 0, 0, 0, 0,
            15, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13,
            14, 15, 16, 255, 218, 0, 8, 1, 1, 0, 4, 0, 0, 176, 33, 156, 111, 35, 105, 27, 72, 218,
            70, 210, 54, 145, 180, 238, 53, 255, 0, 251, 0, 0, 0, 0, 157, 192, 0, 0, 0, 78, 224,
            53, 255, 0, 251, 0, 0, 0, 9, 220, 0, 107, 255, 0, 246, 0, 0, 1, 59, 128, 0, 215, 255,
            0, 236, 0, 0, 63, 255, 217,
        ];

        let mut ifd = Ifd::new();
        ifd.set(tags::NEW_SUBFILE_TYPE, Value::Long(vec![0]));
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![w as u16]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![h as u16]));
        ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![12]));
        ifd.set(tags::COMPRESSION, Value::Short(vec![7])); // lossless JPEG
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
        ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![h as u16]));
        ifd.set(tags::CFA_REPEAT_PATTERN_DIM, Value::Short(vec![2, 2]));
        ifd.set(tags::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2]));
        ifd.set(tags::CFA_PLANE_COLOR, Value::Byte(vec![0, 1, 2]));
        ifd.set(tags::DNG_VERSION, Value::Byte(vec![1, 4, 0, 0]));
        ifd.set(
            tags::UNIQUE_CAMERA_MODEL,
            Value::Ascii("gamut TestCam".to_owned()),
        );
        ifd.set(
            tags::COLOR_MATRIX1,
            Value::SRational(vec![
                (1, 1),
                (0, 1),
                (0, 1),
                (0, 1),
                (1, 1),
                (0, 1),
                (0, 1),
                (0, 1),
                (1, 1),
            ]),
        );
        ifd.set(tags::CALIBRATION_ILLUMINANT1, Value::Short(vec![21])); // D65
        ifd.set(
            tags::AS_SHOT_NEUTRAL,
            Value::Rational(vec![(1, 1), (1, 1), (1, 1)]),
        );
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        ifd.set(
            tags::STRIP_BYTE_COUNTS,
            Value::Long(vec![strip.len() as u32]),
        );

        // Two-pass layout: size the container, then point the strip just past it.
        let single = |ifd: Ifd| TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        };
        let mut dng = write(&single(ifd.clone())).expect("write");
        let strip_at = dng.len() as u32;
        let mut placed = ifd;
        placed.set(tags::STRIP_OFFSETS, Value::Long(vec![strip_at]));
        dng = write(&single(placed)).expect("rewrite");
        dng.extend_from_slice(&strip);

        let ours = DngDecoder::new().decode(&dng).expect("gamut decode");
        assert_eq!(ours.raw.samples(), samples.as_slice());
        let adobe = gamut_dng_oracle::read_raw_dng(&dng).expect("Adobe decode");
        assert_eq!(adobe.samples, samples);
    }
}
