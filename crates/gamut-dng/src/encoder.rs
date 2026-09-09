//! The DNG encoder.

use std::borrow::Cow;

use gamut_core::{Error, Result};
use gamut_ifd::c2pa::{self, C2paExclusions};
use gamut_ifd::{ByteOrder, Ifd, Value, Variant};

use crate::gain_map::ProfileGainTableMap;
use crate::metadata::DngMetadata;
use crate::profile::{CameraProfile, srational, urational};
use crate::raw::{RawImage, RawPhotometry};
use crate::values::{Compression, PhotometricInterpretation};
use crate::writer::{ImageBlocks, write_cfa_dng};
use crate::{bitpack, compression, lossless_jpeg, preview, tags};

/// Encoder for DNG (Adobe Digital Negative) raw images.
///
/// [`encode`](Self::encode) writes a raw image — a CFA mosaic or a demosaiced `LinearRaw` — as a
/// DNG: an IFD 0 holding a small RGB preview plus the camera/colour-profile tags, and a raw
/// sub-IFD holding the full-resolution image. Defaults to little-endian (`II`) classic TIFF;
/// richer compression and metadata are added in later phases (see `STATUS.md`).
#[derive(Debug, Clone)]
pub struct DngEncoder {
    order: ByteOrder,
    dng_version: Option<[u8; 4]>,
    backward_version: [u8; 4],
    big_tiff: bool,
    compression: Compression,
    tiling: Option<(u32, u32)>,
    jxl_distance: f32,
    jxl_effort: u8,
    gain_table_map: Option<ProfileGainTableMap>,
    gain_table_map2: Option<ProfileGainTableMap>,
    metadata: DngMetadata,
    c2pa_reserve: Option<usize>,
}

/// What [`DngEncoder::encode_with_report`] produced: the byte count, and — when the file
/// carries a C2PA manifest store or a reservation for one — the two byte ranges an external
/// signer excludes from its `c2pa.hash.data` binding (C2PA 2.4 §18.5.5).
///
/// `#[non_exhaustive]`: later encoder features may report more without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DngEncodeReport {
    /// The number of bytes appended to the output — the whole DNG.
    pub len: usize,
    /// The C2PA exclusion ranges, as offsets from the first byte of this DNG (not of the
    /// output buffer it was appended to). `None` when no store or reservation was requested.
    ///
    /// `store` is the last range of the file — the store is placed after everything else
    /// (§A.3.6), so a signer overwriting a reservation in place, or replacing the store with one
    /// of a different size, moves no other offset.
    pub c2pa: Option<C2paExclusions>,
}

impl Default for DngEncoder {
    fn default() -> Self {
        Self {
            order: ByteOrder::LittleEndian,
            // None = computed from the features used at encode time (see
            // `required_dng_version`); the backward version (oldest reader that can parse the
            // file) starts at the widely-supported 1.1.0.0 and is raised per the spec's
            // compatibility rules.
            dng_version: None,
            backward_version: [1, 1, 0, 0],
            big_tiff: false,
            compression: Compression::Uncompressed,
            tiling: None,
            // JPEG XL defaults: lossless (distance 0.0) at libjxl's default effort (7, squirrel).
            jxl_distance: 0.0,
            jxl_effort: 7,
            gain_table_map: None,
            gain_table_map2: None,
            metadata: DngMetadata::default(),
            c2pa_reserve: None,
        }
    }
}

impl DngEncoder {
    /// Creates an encoder that writes little-endian (`II`) DNG.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a copy of this encoder that writes in the given byte order.
    #[must_use]
    pub fn with_byte_order(mut self, order: ByteOrder) -> Self {
        self.order = order;
        self
    }

    /// Returns a copy of this encoder that declares the given `DNGVersion` (e.g. `[1, 4, 0, 0]`)
    /// verbatim, overriding the default: the minimal version covering the features actually
    /// written (JPEG XL / `ProfileGainTableMap2` → 1.7.0.0, `ProfileGainTableMap` / the
    /// spectral illuminant → 1.6.0.0, opcodes → their defining version, else 1.4.0.0).
    #[must_use]
    pub fn with_dng_version(mut self, version: [u8; 4]) -> Self {
        self.dng_version = Some(version);
        self
    }

    /// Returns a copy of this encoder that declares the given `DNGBackwardVersion` — the oldest DNG
    /// version a reader needs to fully parse the file.
    ///
    /// If the raw image carries a **non-optional** opcode introduced by a newer DNG version, the
    /// written tag is automatically raised to that opcode's version: a writer must not declare a
    /// backward version below the DNG version of any non-optional opcode present (DNG 1.7.1
    /// Compatibility Issue 7, p. 124).
    #[must_use]
    pub fn with_backward_version(mut self, version: [u8; 4]) -> Self {
        self.backward_version = version;
        self
    }

    /// Returns a copy of this encoder that writes **BigTIFF** (64-bit offsets) instead of classic
    /// TIFF, letting a DNG exceed the 4 GiB classic limit (a DNG 1.7 feature).
    ///
    /// BigTIFF only widens the container's structural fields; every photometry, bit depth, and
    /// profile applies unchanged. A reader detects the variant from the header, so the decoder needs
    /// no flag. Callers should also declare a `DNGVersion`/`DNGBackwardVersion` of at least 1.7.0.0.
    #[must_use]
    pub fn with_big_tiff(mut self, big_tiff: bool) -> Self {
        self.big_tiff = big_tiff;
        self
    }

    /// Returns a copy of this encoder that compresses the raw image with `compression`.
    ///
    /// [`Uncompressed`](Compression::Uncompressed), [`Deflate`](Compression::Deflate) (8/16-bit),
    /// [`LosslessJpeg`](Compression::LosslessJpeg), and — with the `jxl-encode` cargo feature —
    /// [`JpegXl`](Compression::JpegXl) (full-range 16-bit samples) are supported. The preview is
    /// always stored uncompressed.
    #[must_use]
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Returns a copy of this encoder that encodes JPEG XL at the given Butteraugli `distance` —
    /// `0.0` (the default) is lossless; larger values are lossy (1.0 ≈ visually lossless). The
    /// written raw IFD records it in the `JXLDistance` tag. Only meaningful with
    /// [`Compression::JpegXl`].
    #[must_use]
    pub fn with_jxl_distance(mut self, distance: f32) -> Self {
        self.jxl_distance = distance;
        self
    }

    /// Returns a copy of this encoder that encodes JPEG XL at the given libjxl effort level
    /// (`1..=10`; default 7). The written raw IFD records it in the `JXLEffort` tag. Only
    /// meaningful with [`Compression::JpegXl`].
    #[must_use]
    pub fn with_jxl_effort(mut self, effort: u8) -> Self {
        self.jxl_effort = effort;
        self
    }

    /// Returns a copy of this encoder that stores the raw image as a `tile_width × tile_height`
    /// tile grid (`TileOffsets`/`TileByteCounts`) instead of strips.
    ///
    /// Tile dimensions must be positive multiples of 16 (TIFF 6.0 §15); edge tiles are stored
    /// full-size with zero padding, which decoders crop. The tiled layout is how real-world
    /// large raws (e.g. Apple ProRAW) are stored, and it bounds a reader's per-chunk working
    /// set; for small images the per-tile padding overhead usually outweighs that. The preview
    /// stays stripped.
    #[must_use]
    pub fn with_tiling(mut self, tile_width: u32, tile_height: u32) -> Self {
        self.tiling = Some((tile_width, tile_height));
        self
    }

    /// Returns a copy of this encoder that embeds `map` as the raw IFD's `ProfileGainTableMap`
    /// (52525, DNG 1.6). The v1 tag stores 32-bit float gains with no gamma — encoding fails
    /// with a typed error if `map` uses v2-only content.
    #[must_use]
    pub fn with_gain_table_map(mut self, map: ProfileGainTableMap) -> Self {
        self.gain_table_map = Some(map);
        self
    }

    /// Returns a copy of this encoder that embeds `map` as IFD 0's `ProfileGainTableMap2`
    /// (52544, DNG 1.7), the extended form with gamma and integer gain storage. When both maps
    /// are embedded, readers apply only this one (DNG 1.7.1 p. 88).
    #[must_use]
    pub fn with_gain_table_map2(mut self, map: ProfileGainTableMap) -> Self {
        self.gain_table_map2 = Some(map);
        self
    }

    /// Returns a copy of this encoder that embeds `metadata` (EXIF sub-IFD + XMP/IPTC/ICC
    /// blocks, and a caller-computed C2PA manifest store — see [`DngMetadata::c2pa`]).
    #[must_use]
    pub fn with_metadata(mut self, metadata: DngMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Returns a copy of this encoder that reserves `len` zero bytes for a C2PA manifest store
    /// an external signer will fill in afterwards.
    ///
    /// The reservation is written exactly where a store goes — the `C2PA` tag (52545) of IFD 0,
    /// its value last in the file (C2PA 2.4 §A.3.6) — and
    /// [`encode_with_report`](Self::encode_with_report) reports its two exclusion ranges
    /// (§18.5.5). A signer hashes the file around those ranges and overwrites the reservation in
    /// place; nothing else in the file moves. `len` must be at least
    /// [`gamut_ifd::c2pa::MIN_STORE_LEN`] (a JUMBF box header), and a reservation cannot be
    /// combined with a store supplied through [`with_metadata`](Self::with_metadata) — either is
    /// a typed error at encode time.
    #[must_use]
    pub fn with_c2pa_reserved(mut self, len: usize) -> Self {
        self.c2pa_reserve = Some(len);
        self
    }

    /// The C2PA manifest store to write, if any: the caller's, or a zero-filled reservation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if both were requested, if the store is too short to be a
    /// JUMBF box at all ([`c2pa::MIN_STORE_LEN`]), or if it would pack *inline* in this
    /// encoder's container variant — BigTIFF's inline threshold is those same 8 bytes, so a
    /// BigTIFF store must exceed them to be placeable at the end of the file. All caught here,
    /// before any pixel work.
    fn c2pa_store(&self) -> Result<Option<Cow<'_, [u8]>>> {
        let store = match (&self.metadata.c2pa, self.c2pa_reserve) {
            (Some(_), Some(_)) => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: supply either a C2PA manifest store or a reservation, not both",
                ));
            }
            (Some(store), None) => Cow::Borrowed(store.as_slice()),
            (None, Some(len)) => Cow::Owned(vec![0; len]),
            (None, None) => return Ok(None),
        };
        if store.len() < c2pa::MIN_STORE_LEN {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: a C2PA manifest store is at least a JUMBF box header (8 bytes)",
            ));
        }
        // A value no longer than the variant's inline threshold is packed into the entry rather
        // than placed out of line, so it cannot be the run at the end of the file §A.3.6 wants.
        // Only reachable on BigTIFF, whose threshold is the 8 bytes above.
        if store.len() <= self.variant().inline_threshold() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: a BigTIFF C2PA manifest store must exceed 8 bytes, or it packs inline",
            ));
        }
        Ok(Some(store))
    }

    /// The container variant this encoder writes (BigTIFF when [`Self::with_big_tiff`] is set).
    fn variant(&self) -> Variant {
        if self.big_tiff {
            Variant::Big
        } else {
            Variant::Classic
        }
    }

    /// Encodes a raw image — a CFA mosaic or a demosaiced `LinearRaw` — as a DNG, appending the
    /// bytes to `out` and returning the number written.
    ///
    /// `raw` supplies the sensor samples plus the photometry and levels; `profile` supplies the
    /// colour calibration and as-shot white balance. The output is an IFD 0 holding an RGB preview
    /// plus the DNG/profile tags, with the full-resolution image in a `SubIFDs` sub-IFD.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] if the raw is not 3-colour (the profile is a `3 × 3` matrix).
    /// Propagates buffer/validation errors.
    pub fn encode(
        &self,
        raw: &RawImage,
        profile: &CameraProfile,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        self.encode_with_report(raw, profile, out)
            .map(|report| report.len)
    }

    /// [`encode`](Self::encode), also reporting where the C2PA manifest store (or its
    /// reservation, [`with_c2pa_reserved`](Self::with_c2pa_reserved)) landed.
    ///
    /// The store is written last in the file and its entry lives in IFD 0 — the last (and only)
    /// IFD of this encoder's main chain — as C2PA 2.4 §A.3.6 requires. The report's ranges are
    /// offsets from the first byte of the DNG, so a caller appending to a non-empty `out`
    /// rebases them by `out.len()` before the call.
    ///
    /// # Errors
    ///
    /// As [`encode`](Self::encode); additionally [`Error::InvalidInput`] if both a store and a
    /// reservation were configured, or the store is shorter than
    /// [`gamut_ifd::c2pa::MIN_STORE_LEN`].
    pub fn encode_with_report(
        &self,
        raw: &RawImage,
        profile: &CameraProfile,
        out: &mut Vec<u8>,
    ) -> Result<DngEncodeReport> {
        let store = self.c2pa_store()?;
        if color_plane_count(raw) != 3 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "DNG: only 3-colour (RGB) raw images are supported so far",
            ));
        }
        let bits = raw.bits_per_sample();
        // Deflate compresses the *packed* byte stream, and the DNG SDK's reader only accepts it
        // at whole-byte integer depths (8/16/32-bit; dng_read_image::CanReadTile) — a sub-byte
        // deflate raw would be a file the reference reader cannot decode.
        if self.compression == Compression::Deflate && !matches!(bits, 8 | 16) {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "DNG: Deflate compression requires 8- or 16-bit samples",
            ));
        }
        // JPEG XL image data is full-range 16-bit in the DNG ecosystem (readers decode it at
        // pixel-format depth; Apple ProRAW pairs a 10-bit codestream with WhiteLevel 65535).
        // Encoding N-bit code values directly would produce a file the reference SDK decodes
        // scaled — misrendering against the written levels — so sub-16-bit input is rejected:
        // scale the code values and levels to 16-bit first.
        if self.compression == Compression::JpegXl && bits != 16 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "DNG: JPEG XL compression requires full-range 16-bit samples",
            ));
        }
        let (width, height) = (
            raw.dimensions().width as usize,
            raw.dimensions().height as usize,
        );
        let spp = usize::from(raw.samples_per_pixel());

        let raw_data = match self.tiling {
            None => vec![self.encode_chunk(raw.samples(), width, height, spp, bits)?],
            Some((tile_width, tile_height)) => {
                if tile_width == 0
                    || tile_height == 0
                    || tile_width % 16 != 0
                    || tile_height % 16 != 0
                {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "DNG: tile dimensions must be positive multiples of 16",
                    ));
                }
                let (tw, th) = (tile_width as usize, tile_height as usize);
                tile_samples(raw.samples(), width, height, spp, tw, th)
                    .iter()
                    .map(|tile| self.encode_chunk(tile, tw, th, spp, bits))
                    .collect::<Result<Vec<_>>>()?
            }
        };

        let (preview_dims, preview_rgb) = preview::raw_preview(raw);
        // Effective versions: the backward version per the compatibility rules, and (unless
        // overridden) the minimal DNGVersion covering the features used — never below the
        // backward version, which a reader is entitled to assume.
        let backward_version = backward_version_for(self, raw, profile);
        let dng_version = self.dng_version.unwrap_or_else(|| {
            let required = required_dng_version(self, raw, profile);
            if backward_version > required {
                backward_version
            } else {
                required
            }
        });
        let mut ifd0 = self.build_ifd0(profile, preview_dims, dng_version, backward_version)?;
        // The raw-data integrity digest (P17). `gdng_validate` runs ValidateRawImageDigest, so
        // every oracle-gated test enforces this value against the SDK's own computation. For
        // lossy-compressed storage (JPEG XL) the digest covers the compressed chunks; for
        // everything else, the raw samples.
        let digest = if self.compression == Compression::JpegXl {
            crate::digest::lossy_compressed_digest(&raw_data)
        } else {
            crate::digest::new_raw_image_digest(raw)
        };
        ifd0.set(tags::NEW_RAW_IMAGE_DIGEST, Value::Byte(digest.to_vec()));
        // Embed metadata: XMP/IPTC/ICC blocks go in IFD 0; EXIF becomes an `ExifIFD` sub-IFD.
        if !self.metadata.is_empty()
            && let Some(exif) = self.metadata.apply(&mut ifd0)
        {
            ifd0.set_sub_ifd(tags::EXIF_IFD, vec![exif]);
        }
        // The C2PA entry goes in the last IFD of the main chain — IFD 0 here — as an inline
        // placeholder, so the directory layout is final while the store itself is placed after
        // the image data, last in the file (§A.3.6).
        if store.is_some() {
            c2pa::reserve_entry(&mut ifd0);
        }
        let raw_ifd = build_raw_ifd(self, raw)?;

        let preview_blocks = ImageBlocks {
            offset_tag: tags::STRIP_OFFSETS,
            bytecount_tag: tags::STRIP_BYTE_COUNTS,
            blocks: vec![preview_rgb],
        };
        let raw_blocks = match self.tiling {
            None => ImageBlocks {
                offset_tag: tags::STRIP_OFFSETS,
                bytecount_tag: tags::STRIP_BYTE_COUNTS,
                blocks: raw_data,
            },
            Some(_) => ImageBlocks {
                offset_tag: tags::TILE_OFFSETS,
                bytecount_tag: tags::TILE_BYTE_COUNTS,
                blocks: raw_data,
            },
        };

        let mut bytes = write_cfa_dng(
            self.order,
            self.variant(),
            ifd0,
            &preview_blocks,
            raw_ifd,
            &raw_blocks,
        )?;
        let c2pa = match store {
            Some(store) => Some(c2pa::append_store(&mut bytes, &store)?),
            None => None,
        };
        out.extend_from_slice(&bytes);
        Ok(DngEncodeReport {
            len: bytes.len(),
            c2pa,
        })
    }

    /// Builds IFD 0: the RGB preview's image tags plus the DNG version, camera identity, and the
    /// colour-calibration profile. The `SubIFDs` pointer and strip offsets are filled in by the
    /// writer. `backward_version` is the effective (possibly opcode-raised) `DNGBackwardVersion`.
    fn build_ifd0(
        &self,
        profile: &CameraProfile,
        preview_dims: gamut_core::Dimensions,
        dng_version: [u8; 4],
        backward_version: [u8; 4],
    ) -> Result<Ifd> {
        let mut ifd = Ifd::new();
        // Preview image (a reduced-resolution RGB thumbnail).
        ifd.set(tags::NEW_SUBFILE_TYPE, Value::Long(vec![1]));
        ifd.set(tags::IMAGE_WIDTH, count_value(preview_dims.width));
        ifd.set(tags::IMAGE_LENGTH, count_value(preview_dims.height));
        ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8, 8, 8]));
        ifd.set(
            tags::COMPRESSION,
            Value::Short(vec![Compression::Uncompressed.code()]),
        );
        ifd.set(
            tags::PHOTOMETRIC_INTERPRETATION,
            Value::Short(vec![PhotometricInterpretation::Rgb.code()]),
        );
        ifd.set(tags::ORIENTATION, Value::Short(vec![1]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![3]));
        ifd.set(tags::ROWS_PER_STRIP, count_value(preview_dims.height));
        ifd.set(tags::X_RESOLUTION, Value::Rational(vec![(72, 1)]));
        ifd.set(tags::Y_RESOLUTION, Value::Rational(vec![(72, 1)]));
        ifd.set(tags::RESOLUTION_UNIT, Value::Short(vec![2])); // inch
        ifd.set(tags::SOFTWARE, Value::Ascii("gamut-dng".to_owned()));
        ifd.set(
            tags::MODEL,
            Value::Ascii(profile.unique_camera_model().to_owned()),
        );

        // DNG identity + colour profile.
        ifd.set(tags::DNG_VERSION, Value::Byte(dng_version.to_vec()));
        ifd.set(
            tags::DNG_BACKWARD_VERSION,
            Value::Byte(backward_version.to_vec()),
        );
        ifd.set(
            tags::UNIQUE_CAMERA_MODEL,
            Value::Ascii(profile.unique_camera_model().to_owned()),
        );
        ifd.set(
            tags::COLOR_MATRIX1,
            Value::SRational(
                profile
                    .color_matrix1()
                    .iter()
                    .map(|&x| srational(x))
                    .collect(),
            ),
        );
        ifd.set(
            tags::CALIBRATION_ILLUMINANT1,
            Value::Short(vec![profile.calibration_illuminant1().code()]),
        );
        // The two as-shot white-balance tags are mutually exclusive: a profile that records a
        // chromaticity writes `AsShotWhiteXY` and leaves `AsShotNeutral` out, even though the
        // derived neutral is available, because a reader must not have to reconcile two answers.
        if let Some([x, y]) = profile.as_shot_white_xy() {
            ifd.set(
                tags::AS_SHOT_WHITE_XY,
                Value::Rational(vec![urational(x), urational(y)]),
            );
        } else {
            ifd.set(
                tags::AS_SHOT_NEUTRAL,
                Value::Rational(
                    profile
                        .as_shot_neutral()
                        .iter()
                        .map(|&x| urational(x))
                        .collect(),
                ),
            );
        }

        // Optional calibration / profile-identity fields.
        if let Some((matrix2, illuminant2)) = profile.second_illuminant() {
            ifd.set(tags::COLOR_MATRIX2, srational_matrix(matrix2));
            ifd.set(
                tags::CALIBRATION_ILLUMINANT2,
                Value::Short(vec![illuminant2.code()]),
            );
        }
        let (cc1, cc2) = profile.camera_calibration();
        if let Some(cc1) = cc1 {
            ifd.set(tags::CAMERA_CALIBRATION1, srational_matrix(cc1));
        }
        if let Some(cc2) = cc2 {
            ifd.set(tags::CAMERA_CALIBRATION2, srational_matrix(cc2));
        }
        let (fm1, fm2) = profile.forward_matrices();
        if let Some(fm1) = fm1 {
            ifd.set(tags::FORWARD_MATRIX1, srational_matrix(fm1));
        }
        if let Some(fm2) = fm2 {
            ifd.set(tags::FORWARD_MATRIX2, srational_matrix(fm2));
        }
        if let Some(ab) = profile.analog_balance() {
            ifd.set(
                tags::ANALOG_BALANCE,
                Value::Rational(ab.iter().map(|&x| urational(x)).collect()),
            );
        }
        if let Some(stops) = profile.baseline_exposure() {
            ifd.set(
                tags::BASELINE_EXPOSURE,
                Value::SRational(vec![srational(stops)]),
            );
        }
        if let Some(name) = profile.profile_name() {
            ifd.set(tags::PROFILE_NAME, Value::Ascii(name.to_owned()));
        }
        if let Some(policy) = profile.profile_embed_policy() {
            ifd.set(tags::PROFILE_EMBED_POLICY, Value::Long(vec![policy.code()]));
        }
        // The extended gain-table map lives in IFD 0 (its v1 sibling lives in the raw IFD).
        if let Some(map) = &self.gain_table_map2 {
            ifd.set(
                tags::PROFILE_GAIN_TABLE_MAP2,
                Value::Undefined(map.to_bytes_v2(self.order)?),
            );
        }
        Ok(ifd)
    }
}

/// The effective `DNGBackwardVersion`: the configured version, raised to the spec version of
/// every **non-optional** opcode the raw carries (DNG 1.7.1 Compatibility Issue 7, p. 124 — a
/// reader that must execute an opcode needs at least the DNG version that defined it). The same
/// applies to features a reader cannot skip: JPEG XL raw compression requires a 1.7 reader
/// (Compatibility Issue 18), Deflate/lossy-JPEG a 1.4 reader (Issues 10/11), and the
/// spectral-data illuminant (255) a 1.6 reader (Issue 17). Optional content — gain-table maps,
/// masks, depth — deliberately does *not* raise it (Issues 12–15/20). The four version octets
/// compare lexicographically, which matches dotted-version ordering.
fn backward_version_for(encoder: &DngEncoder, raw: &RawImage, profile: &CameraProfile) -> [u8; 4] {
    let mut version = encoder.backward_version;
    let raise = |v: [u8; 4], version: &mut [u8; 4]| {
        if v > *version {
            *version = v;
        }
    };
    let lists = [raw.opcode_list1(), raw.opcode_list2(), raw.opcode_list3()];
    for opcode in lists.iter().flat_map(|l| l.opcodes()) {
        if !opcode.is_optional() {
            raise(opcode.spec_version, &mut version);
        }
    }
    match encoder.compression {
        Compression::JpegXl => raise([1, 7, 0, 0], &mut version),
        Compression::Deflate | Compression::LossyJpeg => raise([1, 4, 0, 0], &mut version),
        _ => {}
    }
    if uses_spectral_illuminant(profile) {
        raise([1, 6, 0, 0], &mut version);
    }
    version
}

/// The minimal `DNGVersion` covering everything this encode writes, used when no explicit
/// version is configured. The floor is 1.4.0.0 (the crate's baseline feature set); JPEG XL and
/// `ProfileGainTableMap2` need 1.7, `ProfileGainTableMap` and illuminant 255 need 1.6, and any
/// opcode (optional included — `DNGVersion` declares what the file *uses*, unlike the backward
/// version) needs its defining version. BigTIFF is independent of the DNG version (spec,
/// "64-bit Format").
fn required_dng_version(encoder: &DngEncoder, raw: &RawImage, profile: &CameraProfile) -> [u8; 4] {
    let mut version = [1, 4, 0, 0];
    let raise = |v: [u8; 4], version: &mut [u8; 4]| {
        if v > *version {
            *version = v;
        }
    };
    if encoder.compression == Compression::JpegXl {
        raise([1, 7, 0, 0], &mut version);
    }
    if encoder.gain_table_map.is_some() {
        raise([1, 6, 0, 0], &mut version);
    }
    if encoder.gain_table_map2.is_some() {
        raise([1, 7, 0, 0], &mut version);
    }
    if uses_spectral_illuminant(profile) {
        raise([1, 6, 0, 0], &mut version);
    }
    let lists = [raw.opcode_list1(), raw.opcode_list2(), raw.opcode_list3()];
    for opcode in lists.iter().flat_map(|l| l.opcodes()) {
        raise(opcode.spec_version, &mut version);
    }
    version
}

/// Whether the profile uses the spectral-data illuminant (`CalibrationIlluminant` 255, DNG 1.6).
fn uses_spectral_illuminant(profile: &CameraProfile) -> bool {
    profile.calibration_illuminant1().code() == 255
        || profile
            .second_illuminant()
            .is_some_and(|(_, illuminant)| illuminant.code() == 255)
}

impl DngEncoder {
    /// Encodes one chunk (a strip or tile) of `cols × rows` pixels at `spp` samples each.
    /// Lossless JPEG and JPEG XL code samples directly; the byte-oriented schemes pack then
    /// compress. Rows are byte-aligned per chunk (each chunk is an independent sample stream),
    /// which is exactly how the decoder consumes them.
    fn encode_chunk(
        &self,
        samples: &[u16],
        cols: usize,
        rows: usize,
        spp: usize,
        bits: u16,
    ) -> Result<Vec<u8>> {
        match self.compression {
            Compression::LosslessJpeg => lossless_jpeg::encode(samples, cols, rows, spp, bits),
            Compression::JpegXl => {
                #[cfg(all(
                    feature = "jxl-encode",
                    any(not(target_arch = "wasm32"), target_os = "emscripten")
                ))]
                {
                    crate::jxl::encode_chunk(
                        samples,
                        cols,
                        rows,
                        spp,
                        self.jxl_distance,
                        self.jxl_effort,
                    )
                }
                #[cfg(not(all(
                    feature = "jxl-encode",
                    any(not(target_arch = "wasm32"), target_os = "emscripten")
                )))]
                {
                    Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "DNG: JPEG XL encoding requires the `jxl-encode` feature (non-wasm)",
                    ))
                }
            }
            _ => {
                let packed = bitpack::pack(samples, bits, cols * spp, self.order);
                compression::compress(self.compression, &packed)
            }
        }
    }
}

/// Splits an image into full-size `tw × th` sample tiles in row-major tile order, zero-padding
/// edge tiles (TIFF 6.0 §15: every stored tile has the same dimensions).
fn tile_samples(
    samples: &[u16],
    width: usize,
    height: usize,
    spp: usize,
    tw: usize,
    th: usize,
) -> Vec<Vec<u16>> {
    let (across, down) = (width.div_ceil(tw), height.div_ceil(th));
    let mut tiles = Vec::with_capacity(across * down);
    for ty in 0..down {
        for tx in 0..across {
            let mut tile = vec![0u16; tw * th * spp];
            let (x0, y0) = (tx * tw, ty * th);
            let copy_cols = tw.min(width - x0);
            for r in 0..th.min(height - y0) {
                let src = ((y0 + r) * width + x0) * spp;
                let dst = r * tw * spp;
                tile[dst..dst + copy_cols * spp]
                    .copy_from_slice(&samples[src..src + copy_cols * spp]);
            }
            tiles.push(tile);
        }
    }
    tiles
}

/// Builds an `SRATIONAL` value from a row-major `3 × 3` colour/calibration matrix.
fn srational_matrix(m: &[f64; 9]) -> Value {
    Value::SRational(m.iter().map(|&x| srational(x)).collect())
}

/// The number of distinct colour planes a raw's photometry carries (`CFAPlaneColor` length for a
/// mosaic, the plane count for a linear image).
fn color_plane_count(raw: &RawImage) -> usize {
    match raw.photometry() {
        RawPhotometry::Cfa { plane_color, .. } => plane_color.len(),
        RawPhotometry::LinearRaw { planes } => usize::from(*planes),
    }
}

/// Builds the raw sub-IFD: the image-data tags, the photometry-specific tags (CFA pattern, or
/// `LinearRaw` planes), and the level model. The strip offsets are filled in by the writer.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the level model cannot be stored: a delta vector whose
/// length doesn't match the active area, a non-integral white level, or a level outside the
/// tag's representable range.
fn build_raw_ifd(encoder: &DngEncoder, raw: &RawImage) -> Result<Ifd> {
    let compression = encoder.compression;
    let mut ifd = Ifd::new();
    let dims = raw.dimensions();
    let spp = raw.samples_per_pixel();
    ifd.set(tags::NEW_SUBFILE_TYPE, Value::Long(vec![0])); // full-resolution main image
    ifd.set(tags::IMAGE_WIDTH, count_value(dims.width));
    ifd.set(tags::IMAGE_LENGTH, count_value(dims.height));
    ifd.set(
        tags::BITS_PER_SAMPLE,
        Value::Short(vec![raw.bits_per_sample(); usize::from(spp)]),
    );
    ifd.set(tags::COMPRESSION, Value::Short(vec![compression.code()]));
    ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![spp]));
    // A tiled image carries the tile geometry; a stripped one carries RowsPerStrip (a strip/tile
    // IFD must not mix the two families).
    match encoder.tiling {
        Some((tile_width, tile_height)) => {
            ifd.set(tags::TILE_WIDTH, count_value(tile_width));
            ifd.set(tags::TILE_LENGTH, count_value(tile_height));
        }
        None => ifd.set(tags::ROWS_PER_STRIP, count_value(dims.height)),
    }
    // JPEG XL records its encode parameters (optional tags; DNG 1.7.1 pp. 97-98).
    if compression == Compression::JpegXl {
        ifd.set(tags::JXL_DISTANCE, Value::Float(vec![encoder.jxl_distance]));
        ifd.set(
            tags::JXL_EFFORT,
            Value::Long(vec![u32::from(encoder.jxl_effort)]),
        );
    }
    // The v1 gain-table map lives in the raw IFD (its v2 sibling lives in IFD 0).
    if let Some(map) = &encoder.gain_table_map {
        ifd.set(
            tags::PROFILE_GAIN_TABLE_MAP,
            Value::Undefined(map.to_bytes_v1(encoder.order)?),
        );
    }
    ifd.set(
        tags::SAMPLE_FORMAT,
        Value::Short(vec![
            crate::values::SampleFormat::UnsignedInteger.code();
            usize::from(spp)
        ]),
    );
    match raw.photometry() {
        RawPhotometry::Cfa {
            repeat,
            pattern,
            plane_color,
            layout,
        } => {
            ifd.set(
                tags::PHOTOMETRIC_INTERPRETATION,
                Value::Short(vec![PhotometricInterpretation::Cfa.code()]),
            );
            ifd.set(
                tags::CFA_REPEAT_PATTERN_DIM,
                Value::Short(vec![repeat.0, repeat.1]),
            );
            ifd.set(tags::CFA_PATTERN, Value::Byte(pattern.clone()));
            ifd.set(tags::CFA_PLANE_COLOR, Value::Byte(plane_color.clone()));
            ifd.set(tags::CFA_LAYOUT, Value::Short(vec![layout.code()]));
        }
        RawPhotometry::LinearRaw { .. } => {
            ifd.set(
                tags::PHOTOMETRIC_INTERPRETATION,
                Value::Short(vec![PhotometricInterpretation::LinearRaw.code()]),
            );
        }
    }
    let levels = raw.levels();
    let (rows, cols) = levels.black_repeat();
    ifd.set(tags::BLACK_LEVEL_REPEAT_DIM, Value::Short(vec![rows, cols]));
    ifd.set(tags::BLACK_LEVEL, black_level_value(levels.black())?);
    ifd.set(tags::WHITE_LEVEL, white_level_value(levels.white())?);
    if let Some(table) = levels.linearization_table() {
        if table.is_empty() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: LinearizationTable must not be empty",
            ));
        }
        ifd.set(tags::LINEARIZATION_TABLE, Value::Short(table.to_vec()));
    }

    // Delta lengths are tied to the active-area geometry (one per column/row, DNG 1.7.1
    // pp. 28-29), which defaults to the full image when the tag is absent.
    let (aa_width, aa_height) = match raw.active_area() {
        Some([top, left, bottom, right]) => (
            right.saturating_sub(left) as usize,
            bottom.saturating_sub(top) as usize,
        ),
        None => (dims.width as usize, dims.height as usize),
    };
    if let Some(deltas) = levels.black_delta_h() {
        if deltas.len() != aa_width {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: BlackLevelDeltaH needs one value per active-area column",
            ));
        }
        ifd.set(tags::BLACK_LEVEL_DELTA_H, delta_value(deltas)?);
    }
    if let Some(deltas) = levels.black_delta_v() {
        if deltas.len() != aa_height {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: BlackLevelDeltaV needs one value per active-area row",
            ));
        }
        ifd.set(tags::BLACK_LEVEL_DELTA_V, delta_value(deltas)?);
    }
    if !raw.masked_areas().is_empty() {
        ifd.set(
            tags::MASKED_AREAS,
            Value::Long(raw.masked_areas().iter().flatten().copied().collect()),
        );
    }
    for (tag, list) in [
        (tags::OPCODE_LIST1, raw.opcode_list1()),
        (tags::OPCODE_LIST2, raw.opcode_list2()),
        (tags::OPCODE_LIST3, raw.opcode_list3()),
    ] {
        if !list.is_empty() {
            ifd.set(tag, Value::Undefined(list.to_bytes()));
        }
    }
    if let Some([t, l, b, r]) = raw.active_area() {
        ifd.set(tags::ACTIVE_AREA, Value::Long(vec![t, l, b, r]));
    }
    if let Some((origin, size)) = raw.default_crop() {
        ifd.set(
            tags::DEFAULT_CROP_ORIGIN,
            Value::Long(vec![origin[0], origin[1]]),
        );
        ifd.set(tags::DEFAULT_CROP_SIZE, Value::Long(vec![size[0], size[1]]));
    }
    Ok(ifd)
}

/// The fixed denominator for fractional levels/deltas: `1 / 65536` steps represent any sub-16-bit
/// fractional level exactly enough for sensor calibration, and keep the numerator of any value
/// below `65536` inside `u32` (and any delta within `±32768` inside `i32`). Fractional values are
/// quantized to this grid when stored.
const LEVEL_DEN: u32 = 65536;

/// Stores a `BlackLevel` pattern: `SHORT`/`LONG` when every value is integral, else `RATIONAL`
/// on the [`LEVEL_DEN`] grid (the three types the tag allows, DNG 1.7.1 p. 28).
fn black_level_value(values: &[f64]) -> Result<Value> {
    let integral = values.iter().all(|v| v.fract() == 0.0);
    if integral && values.iter().all(|v| *v <= f64::from(u16::MAX)) {
        Ok(Value::Short(values.iter().map(|&v| v as u16).collect()))
    } else if integral && values.iter().all(|v| *v <= f64::from(u32::MAX)) {
        Ok(Value::Long(values.iter().map(|&v| v as u32).collect()))
    } else if values.iter().all(|v| *v < f64::from(LEVEL_DEN)) {
        Ok(Value::Rational(
            values
                .iter()
                .map(|&v| ((v * f64::from(LEVEL_DEN)).round() as u32, LEVEL_DEN))
                .collect(),
        ))
    } else {
        Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: fractional black levels must be below 65536",
        ))
    }
}

/// Stores `BlackLevelDeltaH`/`BlackLevelDeltaV` as `SRATIONAL`s on the [`LEVEL_DEN`] grid.
fn delta_value(deltas: &[f64]) -> Result<Value> {
    if deltas.iter().any(|v| !v.is_finite() || v.abs() >= 32768.0) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: black-level deltas must be finite and within +/-32768",
        ));
    }
    Ok(Value::SRational(
        deltas
            .iter()
            .map(|&v| ((v * f64::from(LEVEL_DEN)).round() as i32, LEVEL_DEN as i32))
            .collect(),
    ))
}

/// Stores the per-plane `WhiteLevel`, which has no `RATIONAL` form (DNG 1.7.1 p. 29): values must
/// be integers, stored `SHORT` when they all fit, else `LONG`.
fn white_level_value(values: &[f64]) -> Result<Value> {
    if values
        .iter()
        .any(|v| v.fract() != 0.0 || *v > f64::from(u32::MAX))
    {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: white levels must be integers storable as SHORT or LONG",
        ));
    }
    if values.iter().all(|v| *v <= f64::from(u16::MAX)) {
        Ok(Value::Short(values.iter().map(|&v| v as u16).collect()))
    } else {
        Ok(Value::Long(values.iter().map(|&v| v as u32).collect()))
    }
}

/// Stores a dimension/count as `SHORT` when it fits, else `LONG` (both valid per TIFF 6.0 §2).
fn count_value(n: u32) -> Value {
    if n <= u32::from(u16::MAX) {
        Value::Short(vec![n as u16])
    } else {
        Value::Long(vec![n])
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::Dimensions;
    use gamut_ifd::read_ifd_at;

    use super::*;
    use crate::raw::cfa_color;
    use crate::values::CalibrationIlluminant;

    /// Both halves of the WhiteLevel guard, and the width it picks when the value passes.
    ///
    /// `values.iter().any(|v| v.fract() != 0.0 || *v > u32::MAX)` was only ever fed values that
    /// pass, so neither disjunct was exercised: relaxing the `||` to `&&` accepted a fractional
    /// level, and narrowing the `>` to `==` accepted one past `u32::MAX` (#110). Each needs its
    /// own case, because a value that trips one does not trip the other.
    #[test]
    fn a_white_level_must_be_a_whole_number_that_fits_a_long() {
        let fractional = white_level_value(&[100.5]).expect_err("fractional is not storable");
        assert!(
            fractional.to_string().contains("must be integers"),
            "{fractional}"
        );

        let too_large = white_level_value(&[f64::from(u32::MAX) + 1.0])
            .expect_err("past u32::MAX is not storable");
        assert!(
            too_large.to_string().contains("must be integers"),
            "{too_large}"
        );
    }

    /// The RATIONAL grid's upper bound is exclusive, and has to be.
    ///
    /// A fractional black level is stored as `(v * 65536).round() / 65536`, so a value of exactly
    /// 65536 would need a numerator of 2^32 -- one past `u32::MAX`, where the `as u32` cast
    /// wraps. Relaxing `<` to `<=` admits precisely the value whose numerator does not fit (#110).
    ///
    /// Reaching that arm needs a *mixed* slice: 65536 on its own is integral and is taken by the
    /// LONG arm above, so it never gets here. The 0.5 is what forces the fractional path.
    #[test]
    fn a_fractional_black_level_must_stay_below_the_rational_grid() {
        let err = black_level_value(&[f64::from(LEVEL_DEN), 0.5])
            .expect_err("65536 has no numerator on this grid");
        assert!(err.to_string().contains("must be below 65536"), "{err}");

        // One below the bound, with the same fractional companion, is stored.
        assert!(matches!(
            black_level_value(&[f64::from(LEVEL_DEN) - 1.0, 0.5]),
            Ok(Value::Rational(_))
        ));
    }

    /// The same boundary for `BlackLevel`, which has its own copy of the comparison.
    #[test]
    fn a_black_level_is_stored_short_only_while_it_fits() {
        assert!(matches!(
            black_level_value(&[f64::from(u16::MAX)]),
            Ok(Value::Short(_))
        ));
        assert!(matches!(
            black_level_value(&[f64::from(u16::MAX) + 1.0]),
            Ok(Value::Long(_))
        ));
    }

    /// SHORT while the levels fit, LONG once one does not -- a size choice both DNG readers
    /// accept, so only the file's length records which was taken.
    #[test]
    fn a_white_level_is_stored_short_only_while_it_fits() {
        assert!(matches!(
            white_level_value(&[f64::from(u16::MAX)]),
            Ok(Value::Short(_))
        ));
        assert!(matches!(
            white_level_value(&[f64::from(u16::MAX) + 1.0]),
            Ok(Value::Long(_))
        ));
    }

    #[test]
    fn black_level_delta_rejects_each_invalid_class_independently() {
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            32768.0,
            -32768.0,
        ] {
            let error = delta_value(&[value]).expect_err("invalid delta");
            assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        }
        assert!(delta_value(&[32767.999, -32767.999]).is_ok());
    }

    fn sample_profile() -> CameraProfile {
        // A plausible XYZ->camera matrix and white balance; values are illustrative.
        let m = [0.95, -0.20, -0.05, -0.40, 1.30, 0.10, 0.02, -0.18, 0.85];
        CameraProfile::new(
            "gamut TestCam",
            m,
            CalibrationIlluminant::D65,
            [0.52, 1.0, 0.66],
        )
        .unwrap()
    }

    fn sample_raw(w: u32, h: u32, bits: u16) -> RawImage {
        let pattern = vec![
            cfa_color::RED,
            cfa_color::GREEN,
            cfa_color::GREEN,
            cfa_color::BLUE,
        ];
        let n = (w * h) as usize;
        let max = ((1u32 << bits) - 1) as u16;
        let samples: Vec<u16> = (0..n)
            .map(|i| ((i as u32 * 37) % u32::from(max)) as u16)
            .collect();
        RawImage::new_cfa(
            Dimensions::new(w, h).unwrap(),
            bits,
            (2, 2),
            pattern,
            samples,
        )
        .unwrap()
        .with_black_level(8.0)
        .unwrap()
        .with_white_level(f64::from(max))
        .unwrap()
        .with_active_area([0, 0, h, w])
    }

    fn roundtrip_structure(order: ByteOrder, bits: u16) {
        let raw = sample_raw(8, 6, bits);
        let profile = sample_profile();
        let mut out = Vec::new();
        let n = DngEncoder::new()
            .with_byte_order(order)
            .encode(&raw, &profile, &mut out)
            .expect("encode");
        assert_eq!(n, out.len());

        // The container parses as a TIFF, IFD 0 is the preview + DNG/profile tags.
        let file = gamut_ifd::read(&out).expect("parse DNG");
        assert_eq!(file.order, order);
        assert_eq!(file.ifds.len(), 1, "raw lives in a sub-IFD, not the chain");
        let ifd0 = &file.ifds[0];
        assert_eq!(
            ifd0.get(tags::DNG_VERSION),
            Some(&Value::Byte(vec![1, 4, 0, 0]))
        );
        assert_eq!(
            ifd0.get(tags::UNIQUE_CAMERA_MODEL),
            Some(&Value::Ascii("gamut TestCam".to_owned()))
        );
        assert_eq!(ifd0.get_u32(tags::PHOTOMETRIC_INTERPRETATION), Some(2));
        assert_eq!(ifd0.get_u32(tags::CALIBRATION_ILLUMINANT1), Some(21)); // D65
        if let Some(Value::SRational(m)) = ifd0.get(tags::COLOR_MATRIX1) {
            assert_eq!(m.len(), 9);
            assert!((f64::from(m[0].0) / f64::from(m[0].1) - 0.95).abs() < 1e-4);
        } else {
            panic!("ColorMatrix1 missing/wrong type");
        }

        // Follow the SubIFDs pointer to the raw CFA image.
        let raw_off = ifd0.get_u32(tags::SUB_IFDS).expect("SubIFDs pointer");
        let raw_ifd = read_ifd_at(&out, raw_off.into(), order, Variant::Classic).expect("raw IFD");
        assert_eq!(raw_ifd.get_u32(tags::NEW_SUBFILE_TYPE), Some(0));
        assert_eq!(
            raw_ifd.get_u32(tags::PHOTOMETRIC_INTERPRETATION),
            Some(32803)
        );
        assert_eq!(raw_ifd.get_u32(tags::IMAGE_WIDTH), Some(8));
        assert_eq!(raw_ifd.get_u32(tags::IMAGE_LENGTH), Some(6));
        assert_eq!(
            raw_ifd.get_u32(tags::BITS_PER_SAMPLE),
            Some(u32::from(bits))
        );
        assert_eq!(
            raw_ifd.get(tags::CFA_PATTERN),
            Some(&Value::Byte(vec![0, 1, 1, 2]))
        );
        assert_eq!(
            raw_ifd.get_u32(tags::WHITE_LEVEL),
            Some(raw.levels().white()[0] as u32)
        );
        assert_eq!(raw_ifd.get_u32(tags::BLACK_LEVEL), Some(8));

        // The raw strip bytes deserialize back to the original mosaic.
        let off = raw_ifd.get_u32_vec(tags::STRIP_OFFSETS).expect("offsets")[0] as usize;
        let len = raw_ifd
            .get_u32_vec(tags::STRIP_BYTE_COUNTS)
            .expect("counts")[0] as usize;
        // sample_raw is 8x6, one plane (CFA), so samples_per_row = 8, rows = 6.
        let got = crate::bitpack::unpack(&out[off..off + len], bits, 8, 6, order);
        assert_eq!(got, raw.samples(), "raw samples must round-trip");
    }

    #[test]
    fn cfa_dng_roundtrips_structure_le_16bit() {
        roundtrip_structure(ByteOrder::LittleEndian, 16);
    }

    #[test]
    fn cfa_dng_roundtrips_structure_be_16bit() {
        roundtrip_structure(ByteOrder::BigEndian, 16);
    }

    #[test]
    fn cfa_dng_roundtrips_structure_8bit() {
        roundtrip_structure(ByteOrder::LittleEndian, 8);
    }

    #[test]
    fn cfa_dng_roundtrips_packed_depths() {
        for bits in [10, 12, 14] {
            roundtrip_structure(ByteOrder::LittleEndian, bits);
            roundtrip_structure(ByteOrder::BigEndian, bits);
        }
    }

    #[test]
    fn rejects_non_rgb_inputs() {
        let profile = sample_profile();
        // A 4-plane linear image is not a 3-colour profile target.
        let raw4 =
            RawImage::new_linear_raw(Dimensions::new(4, 4).unwrap(), 16, 4, vec![0; 64]).unwrap();
        assert!(
            DngEncoder::new()
                .encode(&raw4, &profile, &mut Vec::new())
                .is_err()
        );
        // ...but a 12-bit RGB CFA now encodes (packed) fine.
        let raw12 = sample_raw(4, 4, 12);
        assert!(
            DngEncoder::new()
                .encode(&raw12, &profile, &mut Vec::new())
                .is_ok()
        );
    }

    /// Hand-computed golden for the tile splitter: a symmetric split∘assemble round-trip cannot
    /// see a transposed grid or swapped padding, so the expected tiles are written out.
    #[test]
    fn tile_samples_splits_row_major_and_zero_pads_edges() {
        // A 3x3 single-plane image with 2x2 tiles: a 2x2 grid whose right/bottom edges pad.
        let samples: Vec<u16> = (1..=9).collect();
        let tiles = tile_samples(&samples, 3, 3, 1, 2, 2);
        assert_eq!(
            tiles,
            vec![
                vec![1, 2, 4, 5],
                vec![3, 0, 6, 0],
                vec![7, 8, 0, 0],
                vec![9, 0, 0, 0],
            ]
        );
        // Two planes interleave within each tile row.
        let planar: Vec<u16> = (1..=8).collect(); // 2x2 image, 2 planes
        let tiles = tile_samples(&planar, 2, 2, 2, 2, 2);
        assert_eq!(tiles, vec![(1..=8).collect::<Vec<u16>>()]);
    }

    #[test]
    fn encode_rejects_bad_tile_dimensions() {
        let raw = sample_raw(32, 32, 16);
        let profile = sample_profile();
        for (tw, th) in [(0, 32), (32, 0), (24, 32), (32, 40)] {
            let err = DngEncoder::new()
                .with_tiling(tw, th)
                .encode(&raw, &profile, &mut Vec::new())
                .unwrap_err();
            assert!(
                matches!(err, ref error if error.kind() == gamut_core::ErrorKind::InvalidInput && error.static_message().is_some_and(|message| message.contains("multiples of 16"))),
                "({tw}, {th}) must be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn tiled_raw_ifd_carries_tile_tags_not_rows_per_strip() {
        let raw = sample_raw(48, 32, 16);
        let profile = sample_profile();
        let mut out = Vec::new();
        DngEncoder::new()
            .with_tiling(32, 16)
            .encode(&raw, &profile, &mut out)
            .expect("encode");
        let file = gamut_ifd::read(&out).expect("parse");
        let raw_off = file.ifds[0].get_u32(tags::SUB_IFDS).expect("SubIFDs");
        let raw_ifd =
            read_ifd_at(&out, raw_off.into(), file.order, Variant::Classic).expect("raw IFD");
        assert_eq!(raw_ifd.get_u32(tags::TILE_WIDTH), Some(32));
        assert_eq!(raw_ifd.get_u32(tags::TILE_LENGTH), Some(16));
        assert_eq!(raw_ifd.get(tags::ROWS_PER_STRIP), None);
        // 48x32 in 32x16 tiles: 2 across, 2 down.
        assert_eq!(
            raw_ifd.get_u32_vec(tags::TILE_OFFSETS).map(|v| v.len()),
            Some(4)
        );
        assert_eq!(
            raw_ifd.get_u32_vec(tags::TILE_BYTE_COUNTS).map(|v| v.len()),
            Some(4)
        );
        // The preview stays stripped.
        assert!(file.ifds[0].get(tags::STRIP_OFFSETS).is_some());
    }

    /// Encodes with `enc` and returns the written `(DNGVersion, DNGBackwardVersion)`.
    fn versions_of(enc: DngEncoder, raw: &RawImage, profile: &CameraProfile) -> ([u8; 4], [u8; 4]) {
        let mut out = Vec::new();
        enc.encode(raw, profile, &mut out).expect("encode");
        let file = gamut_ifd::read(&out).expect("parse");
        let get = |tag: u16| -> [u8; 4] {
            let Some(Value::Byte(b)) = file.ifds[0].get(tag) else {
                panic!("version tag {tag} missing");
            };
            [b[0], b[1], b[2], b[3]]
        };
        (get(tags::DNG_VERSION), get(tags::DNG_BACKWARD_VERSION))
    }

    /// The exact version table per the spec's compatibility rules — asserted as literal
    /// `[u8; 4]` values so no symmetric transform can hide a wrong raise.
    #[test]
    fn version_computation_follows_the_compatibility_rules() {
        let raw = sample_raw(16, 16, 16);
        let profile = sample_profile();

        // Baseline floor.
        assert_eq!(
            versions_of(DngEncoder::new(), &raw, &profile),
            ([1, 4, 0, 0], [1, 1, 0, 0])
        );
        // Deflate raises the backward version to 1.4 (Compatibility Issue 10).
        assert_eq!(
            versions_of(
                DngEncoder::new().with_compression(crate::values::Compression::Deflate),
                &raw,
                &profile
            ),
            ([1, 4, 0, 0], [1, 4, 0, 0])
        );
        // Gain-table maps raise DNGVersion (1.6 / 1.7) but never the backward version
        // (Issues 15/20).
        let map = ProfileGainTableMap {
            points_v: 1,
            points_h: 1,
            spacing_v: 1.0,
            spacing_h: 1.0,
            origin_v: 0.0,
            origin_h: 0.0,
            points_n: 2,
            input_weights: [0.2; 5],
            gamma: 1.0,
            gain_min: 0.0,
            gain_max: 0.0,
            gains: crate::gain_map::GainValues::F32(vec![1.0, 2.0]),
        };
        assert_eq!(
            versions_of(
                DngEncoder::new().with_gain_table_map(map.clone()),
                &raw,
                &profile
            ),
            ([1, 6, 0, 0], [1, 1, 0, 0])
        );
        let mut map2 = map;
        map2.gains = crate::gain_map::GainValues::U8(vec![0, 255]);
        map2.gain_min = 0.5;
        map2.gain_max = 2.0;
        assert_eq!(
            versions_of(DngEncoder::new().with_gain_table_map2(map2), &raw, &profile),
            ([1, 7, 0, 0], [1, 1, 0, 0])
        );
        // An *optional* opcode raises DNGVersion (the file uses it) but not the backward
        // version; a non-optional one raises both (Issue 7).
        let opcode = |flags: u32| crate::opcode::Opcode {
            id: crate::opcode::opcode_id::TRIM_BOUNDS,
            spec_version: [1, 5, 0, 0],
            flags,
            parameters: vec![0; 16],
        };
        let mut optional = crate::opcode::OpcodeList::default();
        optional.push(opcode(crate::opcode::Opcode::FLAG_OPTIONAL));
        let raw_optional = sample_raw(16, 16, 16).with_opcode_list2(optional);
        assert_eq!(
            versions_of(DngEncoder::new(), &raw_optional, &profile),
            ([1, 5, 0, 0], [1, 1, 0, 0])
        );
        let mut required = crate::opcode::OpcodeList::default();
        required.push(opcode(0));
        let raw_required = sample_raw(16, 16, 16).with_opcode_list2(required);
        assert_eq!(
            versions_of(DngEncoder::new(), &raw_required, &profile),
            ([1, 5, 0, 0], [1, 5, 0, 0])
        );
        // An explicit override is written verbatim.
        assert_eq!(
            versions_of(
                DngEncoder::new().with_dng_version([1, 3, 0, 0]),
                &raw,
                &profile
            ),
            ([1, 3, 0, 0], [1, 1, 0, 0])
        );
        // An explicit backward version above the auto floor lifts DNGVersion with it (a reader
        // may assume DNGVersion >= DNGBackwardVersion).
        assert_eq!(
            versions_of(
                DngEncoder::new().with_backward_version([1, 6, 0, 0]),
                &raw,
                &profile
            ),
            ([1, 6, 0, 0], [1, 6, 0, 0])
        );
        // The spectral-data illuminant (255) needs a 1.6 reader — as the first illuminant...
        let m = [0.9, -0.2, -0.1, -0.4, 1.3, 0.1, 0.0, -0.2, 0.9];
        let spectral1 = CameraProfile::new(
            "gamut SpectralCam",
            m,
            crate::values::CalibrationIlluminant::Other,
            [0.5, 1.0, 0.6],
        )
        .expect("profile");
        assert_eq!(
            versions_of(DngEncoder::new(), &raw, &spectral1),
            ([1, 6, 0, 0], [1, 6, 0, 0])
        );
        // ...and equally when only the *second* illuminant is spectral.
        let spectral2 =
            sample_profile().with_second_illuminant(m, crate::values::CalibrationIlluminant::Other);
        assert_eq!(
            versions_of(DngEncoder::new(), &raw, &spectral2),
            ([1, 6, 0, 0], [1, 6, 0, 0])
        );
        // JPEG XL requires a 1.7 reader: both versions raise (Issue 18).
        #[cfg(feature = "jxl-encode")]
        assert_eq!(
            versions_of(
                DngEncoder::new().with_compression(crate::values::Compression::JpegXl),
                &raw,
                &profile
            ),
            ([1, 7, 0, 0], [1, 7, 0, 0])
        );
    }

    #[test]
    fn count_value_switches_to_long_above_the_u16_boundary() {
        assert_eq!(count_value(0), Value::Short(vec![0]));
        assert_eq!(count_value(65535), Value::Short(vec![65535]));
        assert_eq!(count_value(65536), Value::Long(vec![65536]));
    }

    #[test]
    fn with_compression_preserves_other_builder_state() {
        let enc = DngEncoder::new()
            .with_big_tiff(true)
            .with_compression(crate::values::Compression::Deflate);
        assert!(enc.compression == crate::values::Compression::Deflate);
        // A reset to `Default::default()` would clear big_tiff; the real setter must not.
        assert!(enc.big_tiff, "with_compression must keep earlier settings");
    }
}
