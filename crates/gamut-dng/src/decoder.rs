//! The DNG decoder: parse a DNG back to its raw image, colour profile, and version.
//!
//! This reverses [`crate::encoder`]. It walks the IFD tree (IFD 0 plus the raw sub-IFD reached
//! through `SubIFDs`), decompresses and unpacks the sensor samples, and reconstructs the
//! [`RawImage`] and [`CameraProfile`]. As stated in the crate docs, demosaicing and colour
//! rendering are out of scope — the decoder returns the sensor samples, not a viewable image.

use std::cell::RefCell;

use gamut_core::{Dimensions, Error, Result};
use gamut_ifd::c2pa::{self, C2paExclusions};
use gamut_ifd::{ByteOrder, Ifd, TiffFile, Value, Variant, read, read_ifd_at};
use gamut_metadata::exif::Exif;

use crate::color_profile::{ColorProfileInfo, NoiseProfile, TagSource};
use crate::gain_map::ProfileGainTableMap;
use crate::levels::RawLevels;
use crate::metadata::DngMetadata;
use crate::opcode::OpcodeList;
use crate::profile::CameraProfile;
use crate::raw::RawImage;
use crate::subimage::{
    DepthInfo, MaskSubArea, SemanticMaskInfo, SubImage, SubImageData, SubImageKind,
};
use crate::values::{
    CalibrationIlluminant, Compression, PhotometricInterpretation, ProfileEmbedPolicy, SampleFormat,
};
use crate::{bitpack, compression, lossless_jpeg, tags};

/// One IFD entry preserved verbatim: the tag number and its fully typed [`Value`].
///
/// This is how the decoder represents every field it does not model — private maker tags,
/// DNG features without a typed surface yet — so nothing in the file is silently dropped
/// (issue #109's decode contract). The value is `gamut-ifd`'s typed enum, not opaque bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct RawTag {
    /// The TIFF/DNG tag number.
    pub tag: u16,
    /// The entry's decoded value.
    pub value: Value,
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
    /// The `ExifIFD` arrives whole, as the shared [`Exif`](gamut_metadata::exif::Exif) model —
    /// every entry of the directory, not a chosen subset — so nothing in it needs a separate
    /// verbatim escape hatch.
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
    /// This is one of four verbatim channels that together carry the unmodelled fields of every
    /// directory the decode surfaces: this one, [`raw_extra`](Self::raw_extra),
    /// [`SubImage::extra_tags`] per image directory, and
    /// [`trailing_extra`](Self::trailing_extra) for the last main-chain directory when it is
    /// none of those. Nothing those four reach is silently dropped; `deconstruct` remains the
    /// byte-accounting *diagnostic* view of the same principle, and is what shows the one
    /// residue they do not reach (an interior main-chain page carrying no image — see
    /// [`trailing_extra`](Self::trailing_extra) and `STATUS.md`).
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
    /// Empty for every file this crate writes, which puts the entry in IFD 0.
    ///
    /// **Other pages are not covered.** A main-chain directory that is neither the first nor the
    /// last, carries no image data, and is thus also no sub-image, still reaches no surface; see
    /// `STATUS.md`. That gap predates this field and is not what §A.3.6 creates.
    pub trailing_extra: Vec<RawTag>,
    /// Where the C2PA manifest store in [`metadata.c2pa`](DngMetadata::c2pa) sits in the file:
    /// the two ranges a `c2pa.hash.data` binding excludes (C2PA 2.4 §18.5.5), located by
    /// [`gamut_ifd::c2pa::locate`] in the last IFD of the main chain (§A.3.6). `Some` exactly
    /// when the store is; a `C2PA` tag of a type other than `UNDEFINED` is not a store and stays
    /// in [`ifd0_extra`](Self::ifd0_extra).
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
        let compression =
            Compression::from_code(raw_ifd.get_u32(tags::COMPRESSION).unwrap_or(1) as u16)
                .ok_or_else(|| {
                    Error::unsupported(env!("CARGO_PKG_NAME"), "DNG: unknown compression")
                })?;
        let computed = if compression.is_lossy() {
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
    /// Returns [`Error::InvalidInput`] if the container is malformed or a required tag is missing,
    /// or [`Error::Unsupported`] for a compression scheme or photometry not yet decodable.
    pub fn decode(&self, data: &[u8]) -> Result<DecodedDng> {
        // `read` guarantees at least one IFD, so index 0 below is IFD 0.
        let file = read(data)?;
        let order = file.order;
        let variant = file.variant;

        let (ifds, last_main) = walk_ifds(&file, data);
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
        let c2pa_exclusions = c2pa::locate(data)?;
        let metadata = decode_metadata(
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
            c2pa_exclusions,
        })
    }
}

/// Reconstructs embedded metadata from IFD 0 — the XMP/IPTC/ICC blocks and the EXIF sub-IFD —
/// and the C2PA manifest store from `store_ifd`, the last IFD of the main chain.
///
/// The `ExifIFD` is handed over whole, as the shared [`Exif`] model's Exif sub-IFD: every entry
/// the directory holds survives, so no field of it is "unmodelled" and none is dropped. The DNG's
/// own IFD 0 is *not* copied into the model's 0th IFD — [`DecodedDng`] already carries those
/// fields, typed or as [`ifd0_extra`](DecodedDng::ifd0_extra).
///
/// The store is taken as the `UNDEFINED` bytes C2PA 2.4 §A.3.6 mandates, verbatim — the file's
/// byte order does not apply to them — and only when `located` says
/// [`gamut_ifd::c2pa::locate`] recognised a store in this file. That flag is the *whole*
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
) -> DngMetadata {
    let c2pa = match store_ifd.get(c2pa::C2PA_MANIFEST_STORE) {
        Some(Value::Undefined(store)) if located => Some(store.clone()),
        Some(_) => {
            store_ifd.untouch(c2pa::C2PA_MANIFEST_STORE);
            None
        }
        None => None,
    };
    let exif = ifd0
        .get_u32(tags::EXIF_IFD)
        .and_then(|offset| read_ifd_at(data, u64::from(offset), order, variant).ok())
        .map(|exif_ifd| {
            let mut exif = Exif::new(order);
            exif.set_exif_ifd(exif_ifd);
            exif
        });
    DngMetadata {
        exif,
        xmp: bytes_value(ifd0.get(tags::XMP)),
        iptc: bytes_value(ifd0.get(tags::IPTC_NAA)),
        icc: bytes_value(ifd0.get(tags::ICC_PROFILE)),
        c2pa,
    }
}

/// Extracts the first `(numerator, denominator)` of an unsigned `RATIONAL` value.
fn rational_pair(value: Option<&Value>) -> Option<(u32, u32)> {
    value?.as_rationals()?.first().copied()
}

/// Whether `ifd` holds a raw image (`PhotometricInterpretation` is CFA or LinearRaw).
fn is_raw_ifd(ifd: &Ifd) -> bool {
    matches!(
        ifd.get_u32(tags::PHOTOMETRIC_INTERPRETATION)
            .and_then(|c| u16::try_from(c).ok())
            .and_then(PhotometricInterpretation::from_code),
        Some(PhotometricInterpretation::Cfa | PhotometricInterpretation::LinearRaw)
    )
}

/// An upper bound on the `SubIFDs` nesting depth the decoder follows, bounding hostile pointer
/// graphs; legitimate DNG trees (IFD 0 → raw / enhanced / mask sub-IFDs) are two levels.
const MAX_SUBIFD_DEPTH: usize = 8;

/// Collects every IFD in the file — the top-level chain plus, recursively, every `SubIFDs`
/// child — in encounter order, plus the index of the **last main-chain directory** (where C2PA
/// 2.4 §A.3.6 places the manifest store). Lenient by design: an unreadable child is skipped
/// rather than failing the whole decode, while offset de-duplication and the depth cap terminate
/// hostile pointer cycles.
fn walk_ifds(file: &TiffFile, data: &[u8]) -> (Vec<Ifd>, usize) {
    let mut out = Vec::new();
    let mut visited: Vec<u64> = Vec::new();
    let mut last_main = 0;
    for ifd in &file.ifds {
        // Each main-chain directory is pushed before its descendants, so its index is the
        // length so far; the last one is where C2PA 2.4 §A.3.6 puts the manifest store.
        last_main = out.len();
        collect_sub_ifds(
            ifd,
            data,
            file.order,
            file.variant,
            &mut visited,
            1,
            &mut out,
        );
    }
    (out, last_main)
}

/// Pushes `ifd` and recurses into its `SubIFDs` children (see [`walk_ifds`]).
fn collect_sub_ifds(
    ifd: &Ifd,
    data: &[u8],
    order: ByteOrder,
    variant: Variant,
    visited: &mut Vec<u64>,
    depth: usize,
    out: &mut Vec<Ifd>,
) {
    out.push(ifd.clone());
    if depth >= MAX_SUBIFD_DEPTH {
        return;
    }
    // Offsets are read at full width: a BigTIFF writes `SubIFDs` as `LONG8`, whose values only
    // matter past 4 GiB — exactly where a u32 reading would fail.
    let Some(offsets) = ifd.get_u64_vec(tags::SUB_IFDS) else {
        return;
    };
    for offset in offsets {
        if visited.contains(&offset) {
            continue;
        }
        visited.push(offset);
        if let Ok(sub) = read_ifd_at(data, offset, order, variant) {
            collect_sub_ifds(&sub, data, order, variant, visited, depth + 1, out);
        }
    }
}

/// Selects the full-resolution raw IFD from the walked forest (real DNGs keep the raw in a
/// sub-IFD of IFD 0, but TIFF/EP permits it anywhere).
///
/// Prefers an IFD whose `NewSubFileType` is 0 (the main image; the tag defaults to 0 when
/// absent) with a raw photometry, falling back to any raw-photometry IFD.
fn select_raw_ifd(ifds: &[Ifd]) -> Result<usize> {
    let mut fallback = None;
    for (index, ifd) in ifds.iter().enumerate() {
        if !is_raw_ifd(ifd) {
            continue;
        }
        if ifd.get_u32(tags::NEW_SUBFILE_TYPE).unwrap_or(0) == 0 {
            return Ok(index);
        }
        if fallback.is_none() {
            fallback = Some(index);
        }
    }
    fallback
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: no raw image IFD found"))
}

/// Decodes one non-raw image IFD into a [`SubImage`], or `None` when the IFD carries no image
/// data (or its geometry/chunks are too malformed to represent — `deconstruct` diagnoses those).
///
/// Sub-images are auxiliary, so pixel decoding is **best-effort**: a scheme or stream outside
/// decode scope (baseline-DCT JPEG previews, lossy JPEG, float JXL) falls back to carrying the
/// compressed chunks verbatim rather than failing the whole decode.
fn decode_sub_image(ifd: &TrackedIfd, data: &[u8], order: ByteOrder) -> Option<SubImage> {
    if ifd.get(tags::STRIP_OFFSETS).is_none() && ifd.get(tags::TILE_OFFSETS).is_none() {
        return None;
    }
    let width = ifd.get_u32(tags::IMAGE_WIDTH)?;
    let height = ifd.get_u32(tags::IMAGE_LENGTH)?;
    let dimensions = Dimensions::new(width, height).ok()?;
    let spp = u16::try_from(ifd.get_u32(tags::SAMPLES_PER_PIXEL).unwrap_or(1)).ok()?;
    let bits = ifd
        .get_u32_vec(tags::BITS_PER_SAMPLE)
        .and_then(|v| v.first().copied())
        .unwrap_or(1) as u16;
    let photometric = ifd
        .get_u32(tags::PHOTOMETRIC_INTERPRETATION)
        .and_then(|c| u16::try_from(c).ok())
        .unwrap_or(0);
    let kind = SubImageKind::from_code(ifd.get_u32(tags::NEW_SUBFILE_TYPE).unwrap_or(0));
    let compression = ifd.get_u32(tags::COMPRESSION).unwrap_or(1) as u16;

    let data_payload =
        match decode_image_data(ifd, data, order, width, height, usize::from(spp), bits) {
            Ok(samples) => SubImageData::Decoded(samples),
            Err(_) => SubImageData::Undecoded {
                compression,
                chunks: undecoded_chunks(ifd, data)?,
            },
        };

    let semantic = semantic_mask_info(ifd, dimensions);
    Some(SubImage {
        kind,
        photometric,
        dimensions,
        bits_per_sample: bits,
        samples_per_pixel: spp,
        data: data_payload,
        semantic: if kind == SubImageKind::SemanticMask {
            Some(semantic.unwrap_or_default())
        } else {
            semantic
        },
        // Filled in by `decode` once every consumer has run.
        extra_tags: Vec::new(),
    })
}

/// The stored chunks of an image IFD, verbatim (strips or tiles, in offset order). Only the
/// offset/count tags are consumed; the rest of the layout and interpretation tags
/// (`RowsPerStrip`, `TileWidth`/`TileLength`, `SampleFormat`, interleave factors, …) stay in the
/// extras — the consumer of undecoded chunks needs them to interpret the data.
fn undecoded_chunks(ifd: &TrackedIfd, data: &[u8]) -> Option<Vec<Vec<u8>>> {
    let (offset_tag, count_tag) = if ifd.get(tags::TILE_OFFSETS).is_some() {
        (tags::TILE_OFFSETS, tags::TILE_BYTE_COUNTS)
    } else {
        (tags::STRIP_OFFSETS, tags::STRIP_BYTE_COUNTS)
    };
    let offsets = ifd.get_u64_vec(offset_tag)?;
    let counts = ifd.get_u64_vec(count_tag)?;
    let chunks = byte_chunks(&offsets, &counts, data).ok()?;
    Some(chunks.into_iter().map(<[u8]>::to_vec).collect())
}

/// Reads the semantic-mask tags, when any is present. `MaskSubArea` is validated by pairing top
/// with the mask height and left with the mask width (as the SDK does — the spec's own
/// inequality text transposes the axes) and ignored when invalid, per spec.
fn semantic_mask_info(ifd: &TrackedIfd, mask_dims: Dimensions) -> Option<SemanticMaskInfo> {
    let name = ascii_value(ifd.get(tags::SEMANTIC_NAME));
    let instance_id = ascii_value(ifd.get(tags::SEMANTIC_INSTANCE_ID));
    let sub_area = ifd
        .get_u32_vec(tags::MASK_SUB_AREA)
        .filter(|v| v.len() == 4)
        .and_then(|v| {
            let area = MaskSubArea {
                top: v[0],
                left: v[1],
                full_width: v[2],
                full_height: v[3],
            };
            let fits = u64::from(area.top) + u64::from(mask_dims.height)
                <= u64::from(area.full_height)
                && u64::from(area.left) + u64::from(mask_dims.width) <= u64::from(area.full_width);
            if !fits {
                // Ignored per spec, but not dropped: the rejected value surfaces in the extras.
                ifd.untouch(tags::MASK_SUB_AREA);
            }
            fits.then_some(area)
        });
    if name.is_none() && instance_id.is_none() && sub_area.is_none() {
        return None;
    }
    Some(SemanticMaskInfo {
        name,
        instance_id,
        sub_area,
    })
}

/// Reads IFD 0's depth-map description tags, when any is present.
fn decode_depth_info(ifd0: &TrackedIfd) -> Option<DepthInfo> {
    let short = |tag: u16| ifd0.get_u32(tag).and_then(|v| u16::try_from(v).ok());
    let rational = |tag: u16| rational_pair(ifd0.get(tag));
    let info = DepthInfo {
        format: short(tags::DEPTH_FORMAT),
        near: rational(tags::DEPTH_NEAR),
        far: rational(tags::DEPTH_FAR),
        units: short(tags::DEPTH_UNITS),
        measure_type: short(tags::DEPTH_MEASURE_TYPE),
    };
    (info != DepthInfo::default()).then_some(info)
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
    let bits = if ifd.get_u32(tags::COMPRESSION) == Some(u32::from(Compression::JpegXl.code())) {
        16
    } else {
        bits
    };
    let photometric = ifd
        .get_u32(tags::PHOTOMETRIC_INTERPRETATION)
        .and_then(|c| u16::try_from(c).ok())
        .and_then(PhotometricInterpretation::from_code)
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
                .and_then(crate::values::CfaLayout::from_code)
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
    let repeat = match ifd.get_u32_vec(tags::BLACK_LEVEL_REPEAT_DIM) {
        Some(v) if v.len() == 2 => {
            let rows = u16::try_from(v[0]).ok().filter(|r| *r > 0);
            let cols = u16::try_from(v[1]).ok().filter(|c| *c > 0);
            match (rows, cols) {
                (Some(rows), Some(cols)) => (rows, cols),
                _ => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "DNG: BlackLevelRepeatDim dimensions must be non-zero",
                    ));
                }
            }
        }
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
        None => (dims.width as usize, dims.height as usize),
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

/// How an IFD's image data is chunked: TIFF row-band strips, or the DNG 1.7 tile grid.
#[derive(Debug)]
enum ChunkGrid {
    /// `StripOffsets`/`StripByteCounts` row bands of `rows_per_strip` rows (fewer for the last).
    Strips { rows_per_strip: usize },
    /// `TileOffsets`/`TileByteCounts` over a `TileWidth × TileLength` grid. Every stored tile is
    /// full-size — edge tiles carry padding that assembly crops (TIFF 6.0 §15).
    Tiles {
        tile_width: usize,
        tile_height: usize,
        across: usize,
        down: usize,
    },
}

/// Classifies the IFD's chunk layout: tiled when the tile tags are present, else strips.
fn chunk_grid(ifd: &TrackedIfd, width: usize, height: usize) -> Result<ChunkGrid> {
    if ifd.get(tags::TILE_OFFSETS).is_some() || ifd.get(tags::TILE_WIDTH).is_some() {
        let tile_width = ifd.get_u32(tags::TILE_WIDTH).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: tiled IFD missing TileWidth")
        })? as usize;
        let tile_height = ifd.get_u32(tags::TILE_LENGTH).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: tiled IFD missing TileLength")
        })? as usize;
        if tile_width == 0 || tile_height == 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: tile dimensions must be non-zero",
            ));
        }
        Ok(ChunkGrid::Tiles {
            tile_width,
            tile_height,
            across: width.div_ceil(tile_width),
            down: height.div_ceil(tile_height),
        })
    } else {
        let rows_per_strip = match ifd.get_u32(tags::ROWS_PER_STRIP) {
            Some(0) => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: RowsPerStrip must be non-zero",
                ));
            }
            Some(r) => r as usize,
            None => height,
        };
        Ok(ChunkGrid::Strips { rows_per_strip })
    }
}

/// Decodes an image IFD's chunked pixel data (strips or tiles, any supported compression) into
/// `width * height * spp` unpacked u16 samples. Shared by the raw image and sub-image paths.
fn decode_image_data(
    ifd: &TrackedIfd,
    data: &[u8],
    order: ByteOrder,
    width: u32,
    height: u32,
    spp: usize,
    bits: u16,
) -> Result<Vec<u16>> {
    let compression = Compression::from_code(ifd.get_u32(tags::COMPRESSION).unwrap_or(1) as u16)
        .ok_or_else(|| Error::unsupported(env!("CARGO_PKG_NAME"), "DNG: unknown compression"))?;
    // Reject an undecodable scheme up front, so an empty chunk list cannot mask it.
    if !matches!(
        compression,
        Compression::Uncompressed
            | Compression::Deflate
            | Compression::LosslessJpeg
            | Compression::JpegXl
    ) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: this compression is not yet decodable",
        ));
    }

    // SampleFormat (339) defaults to unsigned integer, the only encoding whose code values the
    // u16 sample model represents. Anything else must fail cleanly, not silently misdecode.
    if let Some(formats) = ifd.get_u32_vec(tags::SAMPLE_FORMAT) {
        for format in formats {
            match u16::try_from(format).ok().and_then(SampleFormat::from_code) {
                Some(SampleFormat::UnsignedInteger) => {}
                Some(SampleFormat::FloatingPoint) => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "DNG: floating-point sample data is not supported",
                    ));
                }
                _ => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "DNG: only unsigned-integer samples are supported",
                    ));
                }
            }
        }
    }

    // PlanarConfiguration (284) defaults to chunky (1), the only layout the interleaved sample
    // model represents. Planar (2) is legal TIFF that a DNG must not use (the SDK's IsValidDNG
    // rejects it) and would otherwise be misread as chunky — silently wrong pixels.
    match ifd.get_u32(tags::PLANAR_CONFIGURATION) {
        None | Some(1) => {}
        Some(2) => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "DNG: planar component storage is not supported",
            ));
        }
        Some(_) => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: PlanarConfiguration must be 1 (chunky) or 2 (planar)",
            ));
        }
    }

    let predictor = crate::predictor::validate(ifd.get_u32(tags::PREDICTOR), compression, bits)?;

    let (width, height) = (width as usize, height as usize);
    let samples_per_row = width
        .checked_mul(spp)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow"))?;
    let expected = samples_per_row
        .checked_mul(height)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow"))?;

    let row_factor = interleave_factor(ifd, tags::ROW_INTERLEAVE_FACTOR, height)?;
    let col_factor = interleave_factor(ifd, tags::COLUMN_INTERLEAVE_FACTOR, width)?;

    let layout = ChunkLayout {
        compression,
        spp,
        bits,
        order,
        predictor,
    };
    let grid = chunk_grid(ifd, width, height)?;
    let chunks = grid_chunks(ifd, data, &grid)?;
    let samples = match grid {
        // Each strip is an independent sample stream of full-width rows; strips concatenate as
        // row bands. Decoding works strip by strip — concatenating packed bytes first would
        // misalign any sub-byte strip whose packed length is not what whole-image geometry
        // predicts (rows are byte-aligned *per strip*).
        ChunkGrid::Strips { rows_per_strip } => {
            let mut samples = Vec::with_capacity(expected);
            let mut remaining_rows = height;
            for chunk in &chunks {
                let rows = rows_per_strip.min(remaining_rows);
                if rows == 0 {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "DNG: more strips than image rows",
                    ));
                }
                samples.extend(decode_chunk_samples(layout, chunk, width, rows)?);
                remaining_rows -= rows;
            }
            if samples.len() != expected {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: raw image data is truncated",
                ));
            }
            samples
        }
        // Tiles decode at full tile size, then blit into place with edge cropping.
        ChunkGrid::Tiles {
            tile_width,
            tile_height,
            across,
            down,
        } => {
            let tile_count = across.checked_mul(down).ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow")
            })?;
            if chunks.len() != tile_count {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: tile count must cover the image grid",
                ));
            }
            let mut samples = vec![0u16; expected];
            for (i, chunk) in chunks.iter().enumerate() {
                let tile = decode_chunk_samples(layout, chunk, tile_width, tile_height)?;
                let x0 = (i % across) * tile_width;
                let y0 = (i / across) * tile_height;
                let copy_cols = tile_width.min(width - x0);
                for r in 0..tile_height {
                    let y = y0 + r;
                    if y >= height {
                        break;
                    }
                    let src = r * tile_width * spp;
                    let dst = (y * width + x0) * spp;
                    samples[dst..dst + copy_cols * spp]
                        .copy_from_slice(&tile[src..src + copy_cols * spp]);
                }
            }
            samples
        }
    };

    // Row/column interleaving (RowInterleaveFactor 50975 / ColumnInterleaveFactor 52547): the
    // stored image concatenates the interleave fields; the logical image re-interleaves them.
    // Applied to the whole assembled image, matching the SDK (dng_read_image::Read reads a full
    // temporary image, then runs Interleave2D over it).
    if row_factor > 1 || col_factor > 1 {
        Ok(deinterleave(
            &samples, width, height, spp, row_factor, col_factor,
        ))
    } else {
        Ok(samples)
    }
}

/// Reads a `RowInterleaveFactor`/`ColumnInterleaveFactor` tag, validating it against the image
/// extent (the SDK rejects factors of 0 or beyond the axis length).
fn interleave_factor(ifd: &TrackedIfd, tag: u16, limit: usize) -> Result<usize> {
    match ifd.get_u32(tag) {
        None => Ok(1),
        Some(f) => {
            let f = f as usize;
            if f == 0 || f > limit {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: interleave factor out of valid range",
                ));
            }
            Ok(f)
        }
    }
}

/// Re-interleaves a stored field-concatenated image into logical pixel order (the decode
/// direction of the SDK's `Interleave2D`).
///
/// With factors `(rf, cf)`, the stored image stacks `rf` row fields (field `i` holds logical
/// rows `r` with `r % rf == i`) and, within each, `cf` column fields likewise; field `i` gets
/// `len / factor` rows/columns, with the first `len % factor` fields one longer. The logical
/// pixel `(r, c)` therefore lives at stored row `offset(r % rf) + r / rf`, column
/// `offset(c % cf) + c / cf`.
fn deinterleave(
    samples: &[u16],
    width: usize,
    height: usize,
    spp: usize,
    row_factor: usize,
    col_factor: usize,
) -> Vec<u16> {
    let field_offset = |index: usize, len: usize, factor: usize| -> usize {
        index * (len / factor) + index.min(len % factor)
    };
    let mut out = vec![0u16; samples.len()];
    for r in 0..height {
        let src_r = field_offset(r % row_factor, height, row_factor) + r / row_factor;
        for c in 0..width {
            let src_c = field_offset(c % col_factor, width, col_factor) + c / col_factor;
            let src = (src_r * width + src_c) * spp;
            let dst = (r * width + c) * spp;
            out[dst..dst + spp].copy_from_slice(&samples[src..src + spp]);
        }
    }
    out
}

/// How one chunk's samples are stored: everything but the bytes themselves. Bundled so the
/// per-chunk decode takes a layout rather than a long positional argument list.
#[derive(Debug, Clone, Copy)]
struct ChunkLayout {
    /// The chunk's compression scheme.
    compression: Compression,
    /// Samples per pixel.
    spp: usize,
    /// Bits per sample.
    bits: u16,
    /// The stream's byte order.
    order: ByteOrder,
    /// The `Predictor` to undo after unpacking.
    predictor: crate::values::Predictor,
}

/// Decodes one chunk (a strip or tile) of `cols x rows` pixels, returning exactly
/// `cols * rows * layout.spp` samples.
fn decode_chunk_samples(
    layout: ChunkLayout,
    chunk: &[u8],
    cols: usize,
    rows: usize,
) -> Result<Vec<u16>> {
    let ChunkLayout {
        compression,
        spp,
        bits,
        order,
        predictor,
    } = layout;
    let want = cols
        .checked_mul(rows)
        .and_then(|n| n.checked_mul(spp))
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow"))?;
    match compression {
        Compression::Uncompressed | Compression::Deflate => {
            // Cap inflation at the packed length this chunk's geometry implies: a Deflate chunk
            // that expands past it cannot be a valid sample stream, and the cap is what keeps a
            // hostile zlib stream from allocating without bound.
            let max_out = cols
                .checked_mul(spp)
                .and_then(|per_row| bitpack::packed_len(per_row, bits, rows))
                .ok_or_else(|| {
                    Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: dimensions overflow")
                })?;
            let bytes = compression::decompress(compression, chunk, max_out)?;
            let mut got = bitpack::unpack(&bytes, bits, cols * spp, rows, order);
            if got.len() < want {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: raw image data is truncated",
                ));
            }
            got.truncate(want); // tolerate chunk padding, per TIFF practice
            // The predictor is undone per chunk, after unpacking and (inside `bitpack`) after the
            // byte-order swap — the SDK's ordering.
            crate::predictor::undo(predictor, &mut got, cols, rows, spp, bits);
            Ok(got)
        }
        // Lossless JPEG decodes samples directly. The JPEG stream's internal width/height/
        // components need not match the chunk's geometry — only the total sample count must
        // (DNG 1.7.1, "Compression": real CFA writers store a two-component stream at half
        // width).
        Compression::LosslessJpeg => {
            let jpeg = lossless_jpeg::decode(chunk)?;
            if jpeg.samples.len() != want {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: lossless-JPEG sample count mismatch",
                ));
            }
            Ok(jpeg.samples)
        }
        // JPEG XL (DNG 1.7): each chunk is a complete bitstream whose geometry/channels must
        // agree with the layout (validated inside the bridge); output is full-range 16-bit,
        // matching the reference SDK — `bits` describes only the codestream's stored precision.
        Compression::JpegXl => crate::jxl::decode_chunk(chunk, cols, rows, spp),
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "DNG: this compression is not yet decodable",
        )),
    }
}

/// Returns the grid's chunks as raw byte slices, in offset-array order. Offsets and counts are
/// read at full 64-bit width (BigTIFF writes them as `LONG8`).
fn grid_chunks<'a>(ifd: &TrackedIfd, data: &'a [u8], grid: &ChunkGrid) -> Result<Vec<&'a [u8]>> {
    let (offset_tag, count_tag, missing) = match grid {
        ChunkGrid::Strips { .. } => (
            tags::STRIP_OFFSETS,
            tags::STRIP_BYTE_COUNTS,
            "DNG: missing StripOffsets/StripByteCounts",
        ),
        ChunkGrid::Tiles { .. } => (
            tags::TILE_OFFSETS,
            tags::TILE_BYTE_COUNTS,
            "DNG: missing TileOffsets/TileByteCounts",
        ),
    };
    let offsets = ifd
        .get_u64_vec(offset_tag)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), missing))?;
    let counts = ifd
        .get_u64_vec(count_tag)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), missing))?;
    byte_chunks(&offsets, &counts, data)
}

/// Resolves parallel offset/byte-count arrays into in-bounds byte slices.
fn byte_chunks<'a>(offsets: &[u64], counts: &[u64], data: &'a [u8]) -> Result<Vec<&'a [u8]>> {
    if offsets.len() != counts.len() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: image-data offset/count length mismatch",
        ));
    }
    let mut chunks = Vec::with_capacity(offsets.len());
    for (&offset, &count) in offsets.iter().zip(counts) {
        let start = usize::try_from(offset).map_err(|_| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: image data out of bounds")
        })?;
        let end = count
            .try_into()
            .ok()
            .and_then(|count: usize| start.checked_add(count))
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: image-data extent overflow")
            })?;
        chunks.push(data.get(start..end).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: image data out of bounds")
        })?);
    }
    Ok(chunks)
}

/// Reconstructs the [`CameraProfile`] from IFD 0's identity and calibration tags, or `None` when
/// the file carries no colour calibration at all.
///
/// `ColorMatrix1` is the tag that decides whether a colour profile exists: a monochrome camera has
/// no colour to calibrate and omits the whole calibration family (a Leica M Monochrom writes a DNG
/// 1.0.0.0 file carrying only `UniqueCameraModel`). Such a file decodes to a raw image with no
/// profile rather than failing — but a calibration that is *present and malformed* is still an
/// error, because that is a broken file rather than an absent feature.
fn decode_profile(ifd0: &TrackedIfd) -> Result<Option<CameraProfile>> {
    // Absent colour calibration is a property of the camera, not a defect. The as-shot white
    // balance may arrive either way — `AsShotNeutral` (50728) or the spec's alternative
    // `AsShotWhiteXY` (50729) — so a file carrying only the chromaticity still yields a profile.
    let white_xy = ifd0.get(tags::AS_SHOT_WHITE_XY).is_some();
    if ifd0.get(tags::COLOR_MATRIX1).is_none()
        || ifd0.get(tags::CALIBRATION_ILLUMINANT1).is_none()
        || (ifd0.get(tags::AS_SHOT_NEUTRAL).is_none() && !white_xy)
    {
        return Ok(None);
    }
    let model = ascii_value(ifd0.get(tags::UNIQUE_CAMERA_MODEL)).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: missing UniqueCameraModel")
    })?;
    let color_matrix1 = matrix9(ifd0, tags::COLOR_MATRIX1)?;
    let illuminant1 = illuminant(ifd0, tags::CALIBRATION_ILLUMINANT1).ok_or_else(|| {
        Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: missing CalibrationIlluminant1",
        )
    })?;
    // The spec makes the two white-balance tags mutually exclusive. A file carrying both is
    // malformed; `AsShotNeutral` wins there, matching the reference implementation, and the
    // chromaticity is dropped rather than left to contradict it.
    let stored_xy = match ifd0.get(tags::AS_SHOT_WHITE_XY) {
        Some(value) if ifd0.get(tags::AS_SHOT_NEUTRAL).is_none() => Some(
            f64_vec(Some(value))
                .filter(|v| v.len() == 2)
                .map(|v| [v[0], v[1]])
                .ok_or_else(|| {
                    Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: malformed AsShotWhiteXY")
                })?,
        ),
        _ => None,
    };
    let neutral = match ifd0.get(tags::AS_SHOT_NEUTRAL) {
        Some(value) => f64_vec(Some(value))
            .filter(|v| v.len() == 3)
            .map(|v| [v[0], v[1], v[2]])
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: malformed AsShotNeutral")
            })?,
        // Replaced below by the DNG 1.7.1 §6 derivation, once the calibration it reads is in
        // place. Never observable: `stored_xy` is `Some` exactly when this arm runs.
        None => [1.0, 1.0, 1.0],
    };

    let mut profile = CameraProfile::new(model, color_matrix1, illuminant1, neutral)?;

    if let (Ok(matrix2), Some(illuminant2)) = (
        matrix9(ifd0, tags::COLOR_MATRIX2),
        illuminant(ifd0, tags::CALIBRATION_ILLUMINANT2),
    ) {
        profile = profile.with_second_illuminant(matrix2, illuminant2);
    }
    if let Ok(cc1) = matrix9(ifd0, tags::CAMERA_CALIBRATION1) {
        profile =
            profile.with_camera_calibration(cc1, matrix9(ifd0, tags::CAMERA_CALIBRATION2).ok());
    }
    if let Ok(fm1) = matrix9(ifd0, tags::FORWARD_MATRIX1) {
        profile = profile.with_forward_matrices(fm1, matrix9(ifd0, tags::FORWARD_MATRIX2).ok());
    }
    if let Some(ab) = f64_vec(ifd0.get(tags::ANALOG_BALANCE)).filter(|v| v.len() == 3) {
        profile = profile.with_analog_balance([ab[0], ab[1], ab[2]]);
    }
    if let Some(be) = f64_vec(ifd0.get(tags::BASELINE_EXPOSURE)).and_then(|v| v.first().copied()) {
        profile = profile.with_baseline_exposure(be);
    }
    if let Some(name) = ascii_value(ifd0.get(tags::PROFILE_NAME)) {
        profile = profile.with_profile_name(name);
    }
    if let Some(policy) = ifd0
        .get_u32(tags::PROFILE_EMBED_POLICY)
        .and_then(ProfileEmbedPolicy::from_code)
    {
        profile = profile.with_profile_embed_policy(policy);
    }
    // Last, so the §6 conversion sees the whole calibration it interpolates over.
    if let Some(xy) = stored_xy {
        profile = profile.with_as_shot_white_xy(xy)?;
    }
    Ok(Some(profile))
}

/// Reads `DNGVersion` as a 4-byte array (defaulting trailing bytes to zero).
fn read_version(ifd0: &TrackedIfd) -> Result<[u8; 4]> {
    let bytes = bytes_value(ifd0.get(tags::DNG_VERSION))
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: missing DNGVersion"))?;
    let mut version = [0u8; 4];
    for (slot, b) in version.iter_mut().zip(bytes) {
        *slot = b;
    }
    Ok(version)
}

/// Extracts a `BYTE`/`UNDEFINED` value's bytes (or a `SHORT` array narrowed to bytes, used for
/// SHORT-typed `CFAPattern` variants — a DNG-specific reading `Value::as_bytes` rightly refuses).
fn bytes_value(value: Option<&Value>) -> Option<Vec<u8>> {
    let value = value?;
    if let Some(b) = value.as_bytes() {
        return Some(b.to_vec());
    }
    match value {
        Value::Short(v) => Some(v.iter().map(|&x| x as u8).collect()),
        _ => None,
    }
}

/// Extracts an `ASCII` (or Exif 3.0 `UTF8`) string value.
fn ascii_value(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(ToOwned::to_owned)
}

/// Divides a rational's parts, mapping a zero denominator to `0.0` (the numeric policy is this
/// codec's, not the container's).
fn ratio(n: f64, d: f64) -> f64 {
    if d == 0.0 { 0.0 } else { n / d }
}

/// Converts a `RATIONAL`/`SRATIONAL` value to `f64`s.
pub(crate) fn f64_vec(value: Option<&Value>) -> Option<Vec<f64>> {
    let value = value?;
    if let Some(r) = value.as_rationals() {
        return Some(r.iter().map(|&(n, d)| ratio(n.into(), d.into())).collect());
    }
    value
        .as_srationals()
        .map(|r| r.iter().map(|&(n, d)| ratio(n.into(), d.into())).collect())
}

/// Converts an unsigned numeric value — `SHORT`, `LONG`, or `RATIONAL`, the three types the
/// `BlackLevel` tag allows (DNG 1.7.1 p. 28) — to `f64`s.
fn unsigned_f64s(value: &Value) -> Option<Vec<f64>> {
    match value {
        Value::Short(v) => Some(v.iter().map(|&x| f64::from(x)).collect()),
        Value::Long(v) => Some(v.iter().map(|&x| f64::from(x)).collect()),
        Value::Rational(r) => Some(r.iter().map(|&(n, d)| ratio(n.into(), d.into())).collect()),
        _ => None,
    }
}

/// Reads a 9-element `(S)RATIONAL` matrix tag as `[f64; 9]`.
fn matrix9(ifd: &TrackedIfd, tag: u16) -> Result<[f64; 9]> {
    let v = f64_vec(ifd.get(tag))
        .filter(|v| v.len() == 9)
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: expected a 3x3 matrix tag")
        })?;
    let mut m = [0.0; 9];
    m.copy_from_slice(&v);
    Ok(m)
}

/// Reads a `CalibrationIlluminant` tag.
fn illuminant(ifd: &TrackedIfd, tag: u16) -> Option<CalibrationIlluminant> {
    ifd.get_u32(tag)
        .and_then(|c| u16::try_from(c).ok())
        .and_then(CalibrationIlluminant::from_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All three types `BlackLevel` allows, and nothing else.
    ///
    /// Only `SHORT` and `RATIONAL` were reached by fixtures, so deleting the `LONG` arm dropped it
    /// to `_ => None` and the suite stayed green (#110) -- a black level stored as `LONG`, which
    /// the spec permits, would have been refused as a bad type.
    #[test]
    fn unsigned_f64s_accepts_short_long_and_rational() {
        assert_eq!(unsigned_f64s(&Value::Short(vec![7])), Some(vec![7.0]));
        assert_eq!(
            unsigned_f64s(&Value::Long(vec![70000])),
            Some(vec![70000.0])
        );
        assert_eq!(
            unsigned_f64s(&Value::Rational(vec![(1, 2)])),
            Some(vec![0.5])
        );
        assert_eq!(unsigned_f64s(&Value::Byte(vec![7])), None);
    }

    #[test]
    fn is_raw_ifd_only_for_raw_photometry() {
        let mut ifd = Ifd::new();
        // An RGB preview IFD (PhotometricInterpretation = 2) is not a raw image.
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![2]));
        assert!(!is_raw_ifd(&ifd));
        // A CFA IFD (32803) is.
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
        assert!(is_raw_ifd(&ifd));
        // A missing tag is not raw either.
        assert!(!is_raw_ifd(&Ifd::new()));
    }

    /// A colour profile needs all three of `ColorMatrix1`, `CalibrationIlluminant1` and an as-shot
    /// white balance. Any one missing means the camera did not calibrate — a monochrome body, say —
    /// and yields `None` rather than a half-built profile or an error.
    #[test]
    fn a_profile_needs_a_matrix_an_illuminant_and_a_white_balance() {
        let matrix = || {
            Value::SRational(vec![
                (1_000_000, 1_000_000),
                (0, 1_000_000),
                (0, 1_000_000),
                (0, 1_000_000),
                (1_000_000, 1_000_000),
                (0, 1_000_000),
                (0, 1_000_000),
                (0, 1_000_000),
                (1_000_000, 1_000_000),
            ])
        };
        let neutral = || {
            Value::Rational(vec![
                (500_000, 1_000_000),
                (1_000_000, 1_000_000),
                (700_000, 1_000_000),
            ])
        };
        let model = || Value::Ascii("gamut TestCam".into());

        // All three present: a profile.
        let mut ifd = Ifd::new();
        ifd.set(tags::UNIQUE_CAMERA_MODEL, model());
        ifd.set(tags::COLOR_MATRIX1, matrix());
        ifd.set(tags::CALIBRATION_ILLUMINANT1, Value::Short(vec![21]));
        ifd.set(tags::AS_SHOT_NEUTRAL, neutral());
        assert!(
            decode_profile(&TrackedIfd::new(&ifd))
                .expect("a complete calibration decodes")
                .is_some()
        );

        // Each one removed in turn: no profile, and no error.
        for missing in [
            tags::COLOR_MATRIX1,
            tags::CALIBRATION_ILLUMINANT1,
            tags::AS_SHOT_NEUTRAL,
        ] {
            let mut partial = Ifd::new();
            partial.set(tags::UNIQUE_CAMERA_MODEL, model());
            if missing != tags::COLOR_MATRIX1 {
                partial.set(tags::COLOR_MATRIX1, matrix());
            }
            if missing != tags::CALIBRATION_ILLUMINANT1 {
                partial.set(tags::CALIBRATION_ILLUMINANT1, Value::Short(vec![21]));
            }
            if missing != tags::AS_SHOT_NEUTRAL {
                partial.set(tags::AS_SHOT_NEUTRAL, neutral());
            }
            assert!(
                decode_profile(&TrackedIfd::new(&partial))
                    .expect("an absent calibration is not an error")
                    .is_none(),
                "a calibration missing tag {missing} must yield no profile"
            );
        }

        // `AsShotWhiteXY` satisfies the white-balance requirement on its own.
        let mut by_xy = Ifd::new();
        by_xy.set(tags::UNIQUE_CAMERA_MODEL, model());
        by_xy.set(tags::COLOR_MATRIX1, matrix());
        by_xy.set(tags::CALIBRATION_ILLUMINANT1, Value::Short(vec![21]));
        by_xy.set(
            tags::AS_SHOT_WHITE_XY,
            Value::Rational(vec![(312_700, 1_000_000), (329_000, 1_000_000)]),
        );
        let profile = decode_profile(&TrackedIfd::new(&by_xy))
            .expect("a chromaticity is a white balance")
            .expect("a profile");
        assert_eq!(profile.as_shot_white_xy(), Some([0.3127, 0.329]));

        // Both tags is malformed; the neutral wins and the chromaticity is dropped, so a re-encode
        // cannot end up writing a white balance that contradicts the one that was read.
        let mut both = by_xy.clone();
        both.set(tags::AS_SHOT_NEUTRAL, neutral());
        let profile = decode_profile(&TrackedIfd::new(&both))
            .expect("both tags decode")
            .expect("a profile");
        assert_eq!(profile.as_shot_white_xy(), None);
        assert_eq!(profile.as_shot_neutral(), &[0.5, 1.0, 0.7]);
    }

    #[test]
    fn bytes_value_extracts_and_narrows() {
        assert_eq!(
            bytes_value(Some(&Value::Byte(vec![1, 2, 3]))),
            Some(vec![1, 2, 3])
        );
        // A SHORT array is narrowed to bytes (used for SHORT-typed CFAPattern variants).
        assert_eq!(
            bytes_value(Some(&Value::Short(vec![0x12, 0x34]))),
            Some(vec![0x12, 0x34])
        );
        assert_eq!(bytes_value(Some(&Value::Long(vec![1]))), None);
        assert_eq!(bytes_value(None), None);
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
        let jpeg = lossless_jpeg::encode(&samples, 4, 2, 1, 12).expect("encode");
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

    /// A reshaped lossless-JPEG stream — half width, doubled components, same total sample
    /// count — decodes, per the spec's total-count rule (a width/components equality would
    /// wrongly reject it).
    #[test]
    fn decode_raw_image_accepts_reshaped_lossless_jpeg_stream() {
        // 8 samples encoded as a 2-wide, 2-component, 2-row stream for a 4x2 one-plane image.
        let samples: Vec<u16> = (0..8).map(|i| i as u16).collect();
        let jpeg = lossless_jpeg::encode(&samples, 2, 2, 2, 12).expect("encode");
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
            Some("DNG: Deflate stream inflates past the expected size")
        );
    }

    /// The `SubIFDs` walk is depth-capped: a 10-deep nested chain yields exactly
    /// `MAX_SUBIFD_DEPTH` IFDs (the cap stops recursion, not collection of the capped node).
    #[test]
    fn walk_ifds_caps_hostile_nesting_depth() {
        // Innermost first: each level wraps the previous as its SubIFDs child.
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        for _ in 0..9 {
            let mut parent = Ifd::new();
            parent.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
            parent.set_sub_ifd(tags::SUB_IFDS, vec![ifd]);
            ifd = parent;
        }
        let bytes = gamut_ifd::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        })
        .expect("write");
        let file = read(&bytes).expect("read");
        assert_eq!(walk_ifds(&file, &bytes).0.len(), MAX_SUBIFD_DEPTH);
    }

    /// The last main-chain index names the *last* top-level directory, not the last directory
    /// collected: sub-IFDs of an earlier page are pushed before a later page, and the later
    /// page's own sub-IFDs after it.
    #[test]
    fn walk_ifds_indexes_the_last_main_chain_directory() {
        let mut child = Ifd::new();
        child.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        let mut page0 = Ifd::new();
        page0.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
        page0.set_sub_ifd(tags::SUB_IFDS, vec![child.clone()]);
        let mut page1 = Ifd::new();
        page1.set(tags::IMAGE_WIDTH, Value::Short(vec![3]));
        page1.set_sub_ifd(tags::SUB_IFDS, vec![child]);
        let bytes = gamut_ifd::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![page0, page1],
        })
        .expect("write");
        let file = read(&bytes).expect("read");
        let (ifds, last_main) = walk_ifds(&file, &bytes);
        // page0, its child, page1, its child.
        assert_eq!(ifds.len(), 4);
        assert_eq!(last_main, 2);
        assert_eq!(
            ifds[last_main].get(tags::IMAGE_WIDTH),
            Some(&Value::Short(vec![3]))
        );
    }

    /// With several raw-photometry IFDs, the main image (`NewSubFileType` 0) wins even when a
    /// reduced-resolution raw appears first.
    #[test]
    fn select_raw_ifd_prefers_the_main_image() {
        let raw_ifd = |nsft: u32, width: u16| {
            let mut ifd = Ifd::new();
            ifd.set(tags::NEW_SUBFILE_TYPE, Value::Long(vec![nsft]));
            ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
            ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![width]));
            ifd
        };
        // A reduced-resolution raw (type 1) first, the main raw (type 0) second.
        let ifds = vec![raw_ifd(1, 2), raw_ifd(0, 4)];
        assert_eq!(select_raw_ifd(&ifds).unwrap(), 1);
        // With no type-0 raw at all, the first raw IFD is the fallback.
        let ifds = vec![raw_ifd(1, 2), raw_ifd(1, 4)];
        assert_eq!(select_raw_ifd(&ifds).unwrap(), 0);
        // No raw photometry anywhere is an error.
        assert!(select_raw_ifd(&[Ifd::new()]).is_err());
    }

    /// `MaskSubArea` validity pairs top with the mask *height* and left with the mask *width*
    /// (the SDK's reading; the spec's inequality text transposes the axes), each axis rejecting
    /// on its own. Boundary values distinguish the additions from any other arithmetic.
    #[test]
    fn mask_sub_area_validation_pairs_axes_exactly() {
        let dims = Dimensions::new(2, 2).unwrap(); // a 2x2 stored mask
        let area_of = |vals: Vec<u32>| {
            let mut ifd = Ifd::new();
            ifd.set(tags::SEMANTIC_NAME, Value::Ascii("m".into()));
            ifd.set(tags::MASK_SUB_AREA, Value::Long(vals));
            semantic_mask_info(&TrackedIfd::new(&ifd), dims).and_then(|s| s.sub_area)
        };
        // top + height against H_full, exactly at the boundary (3 + 2 == 5).
        assert!(area_of(vec![3, 0, 4, 5]).is_some());
        assert!(area_of(vec![4, 0, 4, 5]).is_none());
        // left + width against W_full, exactly at the boundary (3 + 2 == 5).
        assert!(area_of(vec![0, 3, 5, 4]).is_some());
        assert!(area_of(vec![0, 4, 5, 4]).is_none());
        // One failing axis alone invalidates (top overflows, left fits).
        assert!(area_of(vec![3, 0, 4, 4]).is_none());
        // A tall, narrow full mask fits a 2x2 crop — the transposed pairing would reject it.
        assert!(area_of(vec![0, 0, 2, 6]).is_some());
    }

    #[test]
    fn chunk_grid_rejects_zero_tile_dimensions() {
        for (tw, th) in [(0u16, 2u16), (2, 0)] {
            let mut ifd = Ifd::new();
            ifd.set(tags::TILE_WIDTH, Value::Short(vec![tw]));
            ifd.set(tags::TILE_LENGTH, Value::Short(vec![th]));
            ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0]));
            ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![4]));
            let err = chunk_grid(&TrackedIfd::new(&ifd), 4, 4).unwrap_err();
            assert!(
                matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("non-zero"))),
                "({tw}, {th}): {err:?}"
            );
        }
    }

    /// Either tile tag alone classifies the IFD as tiled — the resulting error must come from
    /// the *tile* family, not from a strips fallback complaining about StripOffsets.
    #[test]
    fn either_tile_tag_alone_classifies_as_tiled() {
        // Geometry present, offsets missing.
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::TILE_WIDTH, Value::Short(vec![16]));
        ifd.set(tags::TILE_LENGTH, Value::Short(vec![16]));
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &[],
            ByteOrder::LittleEndian,
            16,
            16,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("Tile"))),
            "{err:?}"
        );
        // Offsets present, geometry missing.
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0]));
        ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![4]));
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &[0; 4],
            ByteOrder::LittleEndian,
            16,
            16,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("TileWidth"))),
            "{err:?}"
        );
    }

    /// Height 4 at RowsPerStrip 2 needs exactly two strips; a third must fail with the
    /// strip-count error (not a generic truncation), pinning the remaining-rows bookkeeping.
    #[test]
    fn decode_image_data_rejects_surplus_strips() {
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![2]));
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0, 8, 16]));
        ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![8, 8, 8]));
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &[0u8; 24],
            ByteOrder::LittleEndian,
            4,
            4,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("more strips"))),
            "{err:?}"
        );
    }

    /// Single-axis interleaving (the Adobe Bayer sample sets *both* factors, so it cannot
    /// distinguish the two conditions): each factor alone must trigger the de-interleave.
    #[test]
    fn single_axis_interleave_deinterleaves() {
        let image = |tag: u16, w: u16, h: u16, data: &[u8]| {
            let mut ifd = Ifd::new();
            ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
            ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![h]));
            ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
            ifd.set(
                tags::STRIP_BYTE_COUNTS,
                Value::Long(vec![u32::from(w) * u32::from(h)]),
            );
            ifd.set(tag, Value::Short(vec![2]));
            decode_image_data(
                &TrackedIfd::new(&ifd),
                data,
                ByteOrder::LittleEndian,
                u32::from(w),
                u32::from(h),
                1,
                8,
            )
            .expect("decode")
        };
        // Row-only: a 1-wide, 4-tall image stores its rows as [r0, r2 | r1, r3].
        assert_eq!(
            image(tags::ROW_INTERLEAVE_FACTOR, 1, 4, &[10, 30, 20, 40]),
            vec![10, 20, 30, 40]
        );
        // Column-only: a 4-wide, 1-tall image stores its columns as [c0, c2 | c1, c3].
        assert_eq!(
            image(tags::COLUMN_INTERLEAVE_FACTOR, 4, 1, &[10, 30, 20, 40]),
            vec![10, 20, 30, 40]
        );
    }

    /// Interleaved *multi-plane* data moves whole pixels: the per-pixel sample pairs stay
    /// together under the column shuffle (a `spp` slip in the flat indexing would tear them).
    #[test]
    fn interleave_moves_whole_pixels_across_planes() {
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![2]));
        ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![1]));
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
        ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![8]));
        ifd.set(tags::COLUMN_INTERLEAVE_FACTOR, Value::Short(vec![2]));
        // Stored pixel pairs in field order [c0, c2 | c1, c3], two samples each.
        let stored = [10u8, 11, 30, 31, 20, 21, 40, 41];
        let got = decode_image_data(
            &TrackedIfd::new(&ifd),
            &stored,
            ByteOrder::LittleEndian,
            4,
            1,
            2,
            8,
        )
        .expect("decode");
        assert_eq!(got, vec![10, 11, 20, 21, 30, 31, 40, 41]);
    }

    /// A factor equal to the axis length is valid (the SDK allows `1..=axis`); one past it is
    /// not. Factor-per-column de-interleaving is the identity, which pins the boundary exactly.
    #[test]
    fn interleave_factor_accepts_the_full_axis() {
        let decode_with_factor = |factor: u16| {
            let mut ifd = Ifd::new();
            ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
            ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![1]));
            ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
            ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![4]));
            ifd.set(tags::COLUMN_INTERLEAVE_FACTOR, Value::Short(vec![factor]));
            decode_image_data(
                &TrackedIfd::new(&ifd),
                &[1, 2, 3, 4],
                ByteOrder::LittleEndian,
                4,
                1,
                1,
                8,
            )
        };
        assert_eq!(
            decode_with_factor(4).expect("factor == width"),
            vec![1, 2, 3, 4]
        );
        assert!(decode_with_factor(5).is_err());
    }

    /// A truncated tile is a typed error before assembly could index out of range.
    #[test]
    fn truncated_tile_data_is_a_typed_error_not_a_panic() {
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::TILE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::TILE_LENGTH, Value::Short(vec![2]));
        ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0]));
        ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![2])); // 2 of the 4 needed bytes
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &[0u8; 2],
            ByteOrder::LittleEndian,
            2,
            2,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("truncated"))),
            "{err:?}"
        );
    }

    #[test]
    fn decode_depth_info_reads_when_present() {
        let mut ifd = Ifd::new();
        assert_eq!(decode_depth_info(&TrackedIfd::new(&ifd)), None);
        ifd.set(tags::DEPTH_FORMAT, Value::Short(vec![2]));
        ifd.set(tags::DEPTH_NEAR, Value::Rational(vec![(1, 10)]));
        ifd.set(tags::DEPTH_UNITS, Value::Short(vec![1]));
        let info = decode_depth_info(&TrackedIfd::new(&ifd)).expect("depth info");
        assert_eq!(info.format, Some(2));
        assert_eq!(info.near, Some((1, 10)));
        assert_eq!(info.far, None);
        assert_eq!(info.units, Some(1));
        assert_eq!(info.measure_type, None);
    }

    /// Hand-computed golden for the SDK's `Interleave2D` decode mapping: fields are concatenated
    /// in storage, with the first `len % factor` fields one row/column longer.
    #[test]
    fn deinterleave_reassembles_fields_row_major() {
        // 4x4, both factors 2: stored rows are [row-field 0 | row-field 1], each with column
        // fields likewise. The stored image below maps back to the logical 0..16 raster.
        let stored = [
            0u16, 2, 1, 3, //
            8, 10, 9, 11, //
            4, 6, 5, 7, //
            12, 14, 13, 15,
        ];
        let logical: Vec<u16> = (0..16).collect();
        assert_eq!(deinterleave(&stored, 4, 4, 1, 2, 2), logical);

        // Uneven split: width 3, column factor 2 — field 0 gets two columns, field 1 one.
        assert_eq!(deinterleave(&[10, 30, 20], 3, 1, 1, 1, 2), vec![10, 20, 30]);

        // Factor 1 on both axes is the identity.
        assert_eq!(deinterleave(&[1, 2, 3, 4], 2, 2, 1, 1, 1), vec![1, 2, 3, 4]);
    }

    #[test]
    fn interleave_factor_validates_range() {
        let mut ifd = Ifd::new();
        assert_eq!(
            interleave_factor(&TrackedIfd::new(&ifd), tags::ROW_INTERLEAVE_FACTOR, 8).unwrap(),
            1
        );
        ifd.set(tags::ROW_INTERLEAVE_FACTOR, Value::Short(vec![2]));
        assert_eq!(
            interleave_factor(&TrackedIfd::new(&ifd), tags::ROW_INTERLEAVE_FACTOR, 8).unwrap(),
            2
        );
        ifd.set(tags::ROW_INTERLEAVE_FACTOR, Value::Short(vec![0]));
        assert!(interleave_factor(&TrackedIfd::new(&ifd), tags::ROW_INTERLEAVE_FACTOR, 8).is_err());
        ifd.set(tags::ROW_INTERLEAVE_FACTOR, Value::Short(vec![9]));
        assert!(interleave_factor(&TrackedIfd::new(&ifd), tags::ROW_INTERLEAVE_FACTOR, 8).is_err());
    }

    /// Hand-computed golden for tile reassembly (the counterpart of the encoder's splitter
    /// golden): a 3x3 image from a 2x2 grid of 2x2 zero-padded tiles, plus the count guard.
    #[test]
    fn decode_image_data_assembles_tiles_with_edge_crop() {
        let tiles: [&[u8]; 4] = [&[1, 2, 4, 5], &[3, 0, 6, 0], &[7, 8, 0, 0], &[9, 0, 0, 0]];
        let data: Vec<u8> = tiles.concat();
        let mut ifd = Ifd::new();
        ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
        ifd.set(tags::TILE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::TILE_LENGTH, Value::Short(vec![2]));
        ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0, 4, 8, 12]));
        ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![4; 4]));

        let samples = decode_image_data(
            &TrackedIfd::new(&ifd),
            &data,
            ByteOrder::LittleEndian,
            3,
            3,
            1,
            8,
        )
        .expect("decode");
        assert_eq!(samples, (1..=9).collect::<Vec<u16>>());

        // A tile list that does not cover the grid is rejected.
        ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0, 4, 8]));
        ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![4; 3]));
        let err = decode_image_data(
            &TrackedIfd::new(&ifd),
            &data,
            ByteOrder::LittleEndian,
            3,
            3,
            1,
            8,
        )
        .unwrap_err();
        assert!(
            matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("tile count"))),
            "expected a tile-count error, got {err:?}"
        );

        // A tiled IFD missing its geometry is rejected.
        ifd.remove(tags::TILE_WIDTH);
        assert!(
            decode_image_data(
                &TrackedIfd::new(&ifd),
                &data,
                ByteOrder::LittleEndian,
                3,
                3,
                1,
                8
            )
            .is_err()
        );
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
}
