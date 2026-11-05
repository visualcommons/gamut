//! The raw sensor image an encoder consumes: the sample buffer plus the photometry (CFA mosaic or
//! demosaiced linear) and the black/white levels needed to interpret it.

use gamut_core::{Dimensions, Error, Result};

use crate::levels::RawLevels;
use crate::linearize::LinearImage;
use crate::opcode::OpcodeList;
use crate::values::CfaLayout;

/// CFA colour codes, as stored in the `CFAPattern` tag (DNG spec / TIFF-EP).
pub mod cfa_color {
    /// Red.
    pub const RED: u8 = 0;
    /// Green.
    pub const GREEN: u8 = 1;
    /// Blue.
    pub const BLUE: u8 = 2;
    /// Cyan.
    pub const CYAN: u8 = 3;
    /// Magenta.
    pub const MAGENTA: u8 = 4;
    /// Yellow.
    pub const YELLOW: u8 = 5;
    /// White.
    pub const WHITE: u8 = 6;
}

/// How a raw image's samples map to colour.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawPhotometry {
    /// A colour-filter-array (mosaic) image: one sample per pixel, its colour given by the
    /// repeating pattern. Stored with `PhotometricInterpretation = CFA` and one sample per pixel.
    Cfa {
        /// The repeat-pattern dimensions `(rows, cols)` (e.g. `(2, 2)` for Bayer).
        repeat: (u16, u16),
        /// The pattern colours, row-major over the repeat tile (length `rows * cols`).
        pattern: Vec<u8>,
        /// The distinct CFA plane colours (e.g. `[R, G, B]`).
        plane_color: Vec<u8>,
        /// The physical CFA layout.
        layout: CfaLayout,
    },
    /// A demosaiced ("linear") image: `planes` interleaved samples per pixel. Stored with
    /// `PhotometricInterpretation = LinearRaw`.
    LinearRaw {
        /// Colour planes per pixel (e.g. 3 for RGB).
        planes: u16,
    },
}

/// A raw sensor image plus the metadata required to store it as a DNG raw sub-IFD.
///
/// Samples are unsigned integers, row-major, `width * height * samples_per_pixel` long — one per
/// pixel for a [`Cfa`](RawPhotometry::Cfa) mosaic, `planes` interleaved per pixel for
/// [`LinearRaw`](RawPhotometry::LinearRaw). The [`RawLevels`] model bounds the linear range
/// (black-level pattern + deltas, per-plane white, optional linearization table).
///
/// Built with [`RawImage::new_cfa`] or [`RawImage::new_linear_raw`] (which validate the buffer and
/// pattern) and refined with the `with_*` setters.
#[derive(Debug, Clone, PartialEq)]
pub struct RawImage {
    dims: Dimensions,
    bits_per_sample: u16,
    samples_per_pixel: u16,
    levels: RawLevels,
    masked_areas: Vec<[u32; 4]>,
    opcode_lists: [OpcodeList; 3],
    active_area: Option<[u32; 4]>,
    default_crop: Option<([u32; 2], [u32; 2])>,
    photometry: RawPhotometry,
    samples: Vec<u16>,
}

impl RawImage {
    /// Creates a CFA (mosaic) raw image from a single-plane `samples` buffer.
    ///
    /// `cfa_repeat` is `(rows, cols)` of the repeating pattern tile and `cfa_pattern` lists its
    /// colours row-major (length `rows * cols`). Defaults: black `0`, white `2^bits - 1`, plane
    /// colours `[R, G, B]`, rectangular layout, full active area.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `bits_per_sample` is not in `1..=16`, the pattern length
    /// does not match `cfa_repeat`, or `samples.len()` is not `width * height`.
    pub fn new_cfa(
        dims: Dimensions,
        bits_per_sample: u16,
        cfa_repeat: (u16, u16),
        cfa_pattern: Vec<u8>,
        samples: Vec<u16>,
    ) -> Result<Self> {
        check_bits(bits_per_sample)?;
        let (rr, rc) = cfa_repeat;
        if rr == 0 || rc == 0 || cfa_pattern.len() != usize::from(rr) * usize::from(rc) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: CFA pattern length must equal cfa_repeat rows * cols",
            ));
        }
        check_sample_count(dims, 1, &samples)?;
        Ok(Self {
            dims,
            bits_per_sample,
            samples_per_pixel: 1,
            levels: RawLevels::uniform(1, 0.0, white_level_default(bits_per_sample))?,
            masked_areas: Vec::new(),
            opcode_lists: [OpcodeList::new(), OpcodeList::new(), OpcodeList::new()],
            active_area: None,
            default_crop: None,
            photometry: RawPhotometry::Cfa {
                repeat: cfa_repeat,
                pattern: cfa_pattern,
                plane_color: vec![cfa_color::RED, cfa_color::GREEN, cfa_color::BLUE],
                layout: CfaLayout::Rectangular,
            },
            samples,
        })
    }

    /// Creates a demosaiced ("linear") raw image of `planes` interleaved samples per pixel.
    ///
    /// Defaults: black `0`, white `2^bits - 1`, full active area.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `bits_per_sample` is not in `1..=16`, `planes` is zero,
    /// or `samples.len()` is not `width * height * planes`.
    pub fn new_linear_raw(
        dims: Dimensions,
        bits_per_sample: u16,
        planes: u16,
        samples: Vec<u16>,
    ) -> Result<Self> {
        check_bits(bits_per_sample)?;
        if planes == 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: LinearRaw needs at least one plane",
            ));
        }
        check_sample_count(dims, planes, &samples)?;
        Ok(Self {
            dims,
            bits_per_sample,
            samples_per_pixel: planes,
            levels: RawLevels::uniform(planes, 0.0, white_level_default(bits_per_sample))?,
            masked_areas: Vec::new(),
            opcode_lists: [OpcodeList::new(), OpcodeList::new(), OpcodeList::new()],
            active_area: None,
            default_crop: None,
            photometry: RawPhotometry::LinearRaw { planes },
            samples,
        })
    }

    /// Replaces the level model (black pattern + deltas, per-plane white, linearization table).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `levels` describes a different number of sample planes
    /// than this image has.
    pub fn with_levels(mut self, levels: RawLevels) -> Result<Self> {
        if levels.samples_per_pixel() != self.samples_per_pixel {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: levels plane count must match the image's samples per pixel",
            ));
        }
        self.levels = levels;
        Ok(self)
    }

    /// Sets a uniform black level (the zero-light encoding value) across every pattern cell and
    /// plane, resetting any black pattern or deltas but keeping the white levels and
    /// linearization table.
    ///
    /// For per-cell/per-plane black levels use [`RawLevels`] with [`Self::with_levels`].
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `black_level` is negative or not finite.
    pub fn with_black_level(mut self, black_level: f64) -> Result<Self> {
        let spp = usize::from(self.samples_per_pixel);
        let mut levels = RawLevels::new(
            self.samples_per_pixel,
            (1, 1),
            vec![black_level; spp],
            self.levels.white().to_vec(),
        )?;
        if let Some(table) = self.levels.linearization_table() {
            levels = levels.with_linearization_table(table.to_vec());
        }
        self.levels = levels;
        Ok(self)
    }

    /// Sets a uniform white level (the saturated encoding value) for every plane, keeping the
    /// rest of the level model.
    ///
    /// For per-plane white levels use [`RawLevels`] with [`Self::with_levels`].
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `white_level` is not finite and positive.
    pub fn with_white_level(mut self, white_level: f64) -> Result<Self> {
        let spp = usize::from(self.samples_per_pixel);
        let mut levels = RawLevels::new(
            self.samples_per_pixel,
            self.levels.black_repeat(),
            self.levels.black().to_vec(),
            vec![white_level; spp],
        )?;
        if let Some(dh) = self.levels.black_delta_h() {
            levels = levels.with_black_delta_h(dh.to_vec());
        }
        if let Some(dv) = self.levels.black_delta_v() {
            levels = levels.with_black_delta_v(dv.to_vec());
        }
        if let Some(table) = self.levels.linearization_table() {
            levels = levels.with_linearization_table(table.to_vec());
        }
        self.levels = levels;
        Ok(self)
    }

    /// Sets the `MaskedAreas` rectangles `[top, left, bottom, right]` — fully-masked sensor
    /// regions a processor may use to estimate black levels. Returns `self` for chaining.
    #[must_use]
    pub fn with_masked_areas(mut self, areas: Vec<[u32; 4]>) -> Self {
        self.masked_areas = areas;
        self
    }

    /// Sets `OpcodeList1` — opcodes applied to the **stored** raw data, before linearization.
    /// Returns `self` for chaining.
    #[must_use]
    pub fn with_opcode_list1(mut self, list: OpcodeList) -> Self {
        self.opcode_lists[0] = list;
        self
    }

    /// Sets `OpcodeList2` — opcodes applied after the raw-to-linear mapping. Returns `self` for
    /// chaining.
    #[must_use]
    pub fn with_opcode_list2(mut self, list: OpcodeList) -> Self {
        self.opcode_lists[1] = list;
        self
    }

    /// Sets `OpcodeList3` — opcodes applied after demosaicing. Returns `self` for chaining.
    #[must_use]
    pub fn with_opcode_list3(mut self, list: OpcodeList) -> Self {
        self.opcode_lists[2] = list;
        self
    }

    /// Sets the active-area rectangle `[top, left, bottom, right]` (the region holding image data,
    /// excluding masked/border pixels). Returns `self` for chaining.
    #[must_use]
    pub fn with_active_area(mut self, active_area: [u32; 4]) -> Self {
        self.active_area = Some(active_area);
        self
    }

    /// Sets the default-crop rectangle as `(origin, size)` in pixels relative to the active area —
    /// the region a renderer crops to by default (`DefaultCropOrigin` / `DefaultCropSize`). Returns
    /// `self` for chaining.
    #[must_use]
    pub fn with_default_crop(mut self, origin: [u32; 2], size: [u32; 2]) -> Self {
        self.default_crop = Some((origin, size));
        self
    }

    /// Sets the distinct CFA plane colours (no effect on a linear image). Returns `self`.
    #[must_use]
    pub fn with_cfa_plane_color(mut self, colors: Vec<u8>) -> Self {
        if let RawPhotometry::Cfa { plane_color, .. } = &mut self.photometry {
            *plane_color = colors;
        }
        self
    }

    /// Sets the CFA layout (no effect on a linear image). Returns `self`.
    #[must_use]
    pub fn with_cfa_layout(mut self, cfa_layout: CfaLayout) -> Self {
        if let RawPhotometry::Cfa { layout, .. } = &mut self.photometry {
            *layout = cfa_layout;
        }
        self
    }

    /// The sensor sample dimensions.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        self.dims
    }

    /// Bits per stored sample.
    #[must_use]
    pub fn bits_per_sample(&self) -> u16 {
        self.bits_per_sample
    }

    /// Samples per pixel (1 for a CFA mosaic, the plane count for a linear image).
    #[must_use]
    pub fn samples_per_pixel(&self) -> u16 {
        self.samples_per_pixel
    }

    /// The level model: the black pattern (+ deltas), per-plane white, and linearization table.
    #[must_use]
    pub fn levels(&self) -> &RawLevels {
        &self.levels
    }

    /// The `MaskedAreas` rectangles `[top, left, bottom, right]` (empty when none are declared).
    #[must_use]
    pub fn masked_areas(&self) -> &[[u32; 4]] {
        &self.masked_areas
    }

    /// `OpcodeList1`: opcodes applied to the stored raw data (empty when absent). This crate
    /// parses and round-trips the container; *applying* opcodes is the caller's job for now
    /// (`STATUS.md` P18).
    #[must_use]
    pub fn opcode_list1(&self) -> &OpcodeList {
        &self.opcode_lists[0]
    }

    /// `OpcodeList2`: opcodes applied after the raw-to-linear mapping (empty when absent).
    #[must_use]
    pub fn opcode_list2(&self) -> &OpcodeList {
        &self.opcode_lists[1]
    }

    /// `OpcodeList3`: opcodes applied after demosaicing (empty when absent).
    #[must_use]
    pub fn opcode_list3(&self) -> &OpcodeList {
        &self.opcode_lists[2]
    }

    /// The active-area rectangle `[top, left, bottom, right]`, if set.
    #[must_use]
    pub fn active_area(&self) -> Option<[u32; 4]> {
        self.active_area
    }

    /// The default-crop `(origin, size)` in pixels, if set.
    #[must_use]
    pub fn default_crop(&self) -> Option<([u32; 2], [u32; 2])> {
        self.default_crop
    }

    /// The image's photometry (CFA mosaic or linear).
    #[must_use]
    pub fn photometry(&self) -> &RawPhotometry {
        &self.photometry
    }

    /// The samples, row-major, `width * height * samples_per_pixel` long.
    #[must_use]
    pub fn samples(&self) -> &[u16] {
        &self.samples
    }

    /// Maps the stored sensor values to **linear reference values** — the DNG 1.7.1 Chapter-5
    /// pipeline: linearization-table lookup, black subtraction (pattern anchored at the active
    /// area, plus the per-column/per-row deltas), rescaling so the white level maps to `1.0`,
    /// and clipping to `[0.0, 1.0]`.
    ///
    /// The output is the **active-area crop** (the same geometry as the Adobe SDK's stage-2
    /// image, against which this mapping is differentially tested). Demosaicing and colour
    /// rendering remain out of scope — a CFA input stays a mosaic, just linearized.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the active area is empty or out of bounds, a delta
    /// vector doesn't match the active area, the linearization table is empty, or a plane's
    /// white level does not exceed its maximum computed black level.
    pub fn to_linear(&self) -> Result<LinearImage> {
        crate::linearize::linearize(self)
    }

    /// Computes this image's **`NewRawImageDigest`** (tag 51111) in the raw-sample domain: the
    /// Adobe SDK's MD5-over-raw-image algorithm — a 256×256 digest-tile grid, per-tile planar
    /// little-endian serialisation (bytes for images of ≤ 8 bits or with a ≤ 256-entry
    /// linearization table), and a final MD5 over the tile digests — reproduced bit-exactly and
    /// differentially gated against the SDK.
    ///
    /// The encoder writes this digest into files with lossless storage; compare it against a
    /// decoded file's
    /// [`DecodedDng::new_raw_image_digest`](crate::DecodedDng::new_raw_image_digest) to verify
    /// raw-data integrity. **Lossy-compressed** storage (JPEG XL, lossy JPEG) instead digests
    /// the compressed chunks (the SDK's `dng_lossy_compressed_image::FindDigest`), so for such
    /// files the stored tag will not equal this sample-domain value.
    #[must_use]
    pub fn new_raw_image_digest(&self) -> [u8; 16] {
        crate::digest::new_raw_image_digest(self)
    }
}

/// Validates a bit depth is in the storable range.
fn check_bits(bits: u16) -> Result<()> {
    if (1..=16).contains(&bits) {
        Ok(())
    } else {
        Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: bits_per_sample must be 1..=16",
        ))
    }
}

/// Validates `samples.len()` equals `width * height * spp`.
fn check_sample_count(dims: Dimensions, spp: u16, samples: &[u16]) -> Result<()> {
    let expected = dims
        .num_pixels()
        .and_then(|p| p.checked_mul(usize::from(spp)))
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: image dimensions overflow")
        })?;
    if samples.len() == expected {
        Ok(())
    } else {
        Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: sample count must equal width * height * samples_per_pixel",
        ))
    }
}

/// The default white level for `bits` bits per sample: `2^bits - 1`.
fn white_level_default(bits: u16) -> f64 {
    f64::from((1u32 << bits) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(w: u32, h: u32) -> Dimensions {
        Dimensions::new(w, h).unwrap()
    }

    #[test]
    fn new_cfa_validates_and_defaults() {
        let raw = RawImage::new_cfa(
            dims(4, 4),
            16,
            (2, 2),
            vec![
                cfa_color::RED,
                cfa_color::GREEN,
                cfa_color::GREEN,
                cfa_color::BLUE,
            ],
            vec![0u16; 16],
        )
        .expect("valid");
        assert_eq!(raw.levels().white(), &[65535.0]);
        assert_eq!(raw.levels().black(), &[0.0]);
        assert_eq!(raw.samples_per_pixel(), 1);
        assert!(matches!(raw.photometry(), RawPhotometry::Cfa { .. }));
        assert!(raw.masked_areas().is_empty());
    }

    #[test]
    fn new_linear_raw_validates() {
        let raw = RawImage::new_linear_raw(dims(2, 2), 16, 3, vec![0u16; 12]).expect("valid");
        assert_eq!(raw.samples_per_pixel(), 3);
        assert_eq!(raw.photometry(), &RawPhotometry::LinearRaw { planes: 3 });
        // Wrong sample count (needs w*h*planes = 12).
        assert!(RawImage::new_linear_raw(dims(2, 2), 16, 3, vec![0; 11]).is_err());
        assert!(RawImage::new_linear_raw(dims(2, 2), 16, 0, vec![]).is_err());
    }

    #[test]
    fn new_cfa_rejects_bad_sizes() {
        assert!(RawImage::new_cfa(dims(4, 4), 16, (2, 2), vec![0, 1, 1, 2], vec![0; 15]).is_err());
        assert!(RawImage::new_cfa(dims(4, 4), 16, (2, 2), vec![0, 1, 1], vec![0; 16]).is_err());
        assert!(RawImage::new_cfa(dims(4, 4), 17, (2, 2), vec![0, 1, 1, 2], vec![0; 16]).is_err());
    }

    #[test]
    fn setters_chain() {
        let raw = RawImage::new_cfa(dims(2, 2), 12, (2, 2), vec![0, 1, 1, 2], vec![0; 4])
            .unwrap()
            .with_black_level(64.0)
            .unwrap()
            .with_white_level(4095.0)
            .unwrap()
            .with_active_area([0, 0, 2, 2])
            .with_masked_areas(vec![[0, 0, 2, 1]])
            .with_cfa_layout(CfaLayout::Rectangular);
        assert_eq!(raw.levels().black(), &[64.0]);
        assert_eq!(raw.levels().white(), &[4095.0]);
        assert_eq!(raw.active_area(), Some([0, 0, 2, 2]));
        assert_eq!(raw.masked_areas(), &[[0, 0, 2, 1]]);
        // Invalid uniform levels are rejected, not silently ignored.
        assert!(raw.clone().with_black_level(-1.0).is_err());
        assert!(raw.clone().with_white_level(0.0).is_err());
    }

    #[test]
    fn with_levels_validates_plane_count_and_keeps_model_pieces() {
        let raw = RawImage::new_linear_raw(dims(2, 2), 12, 3, vec![0; 12]).unwrap();
        // A 2-plane model cannot attach to a 3-plane image.
        let two_plane = RawLevels::uniform(2, 0.0, 4095.0).unwrap();
        assert!(raw.clone().with_levels(two_plane).is_err());
        // A full 3-plane model round-trips through the getter.
        let levels = RawLevels::new(3, (1, 1), vec![1.0, 2.0, 3.0], vec![100.0, 200.0, 300.0])
            .unwrap()
            .with_black_delta_v(vec![0.5, -0.5])
            .with_linearization_table(vec![0, 2, 4]);
        let raw = raw.with_levels(levels.clone()).unwrap();
        assert_eq!(raw.levels(), &levels);
        // Uniform white reset keeps the black pattern, deltas, and table.
        let raw = raw.with_white_level(4000.0).unwrap();
        assert_eq!(raw.levels().black(), &[1.0, 2.0, 3.0]);
        assert_eq!(raw.levels().black_delta_v(), Some(&[0.5, -0.5][..]));
        assert_eq!(raw.levels().linearization_table(), Some(&[0, 2, 4][..]));
        assert_eq!(raw.levels().white(), &[4000.0, 4000.0, 4000.0]);
        // Uniform black reset drops the pattern/deltas but keeps white and the table.
        let raw = raw.with_black_level(8.0).unwrap();
        assert_eq!(raw.levels().black_repeat(), (1, 1));
        assert_eq!(raw.levels().black(), &[8.0, 8.0, 8.0]);
        assert_eq!(raw.levels().black_delta_v(), None);
        assert_eq!(raw.levels().linearization_table(), Some(&[0, 2, 4][..]));
    }

    #[test]
    fn new_cfa_validates_repeat_dimensions() {
        // A zero repeat dimension is rejected even though an empty pattern "matches" rows*cols = 0.
        assert!(RawImage::new_cfa(dims(2, 2), 8, (0, 2), vec![], vec![0; 4]).is_err());
        assert!(RawImage::new_cfa(dims(2, 2), 8, (2, 0), vec![], vec![0; 4]).is_err());
        // A non-square repeat: the pattern length must be rows * cols (6), not rows + cols (5).
        let pattern = vec![0u8, 1, 1, 2, 0, 1];
        assert!(RawImage::new_cfa(dims(3, 2), 8, (2, 3), pattern, vec![0; 6]).is_ok());
        assert!(RawImage::new_cfa(dims(3, 2), 8, (2, 3), vec![0; 5], vec![0; 6]).is_err());
    }
}
