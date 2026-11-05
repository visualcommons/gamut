//! Coded-plane bit depth and chroma subsampling.
//!
//! These describe a codec's *coded* planes, distinct from an interleaved buffer's layout — that is
//! the [`Pixel`](gamut_core::Pixel) vocabulary (`Rgb8`, `Rgba8`, …) in `gamut-core`. [`BitDepth`] is
//! wired into the AV1 reconstruction; [`ChromaSubsampling`] models all four AV1 layouts plus
//! JPEG's 4:4:0 and carries the plane geometry ([`ChromaSubsampling::chroma_dimensions`]) that
//! [`Planar8`](crate::Planar8) uses. The four AV1 layouts are coded end to end by `gamut-avif`;
//! `Cs400` (monochrome) is the grey and alpha planes of both [`Planar8`](crate::Planar8) and
//! [`Planar16`](crate::Planar16).

/// Bits per sample of a coded plane.
///
/// `#[non_exhaustive]`: models the AV1 profile depths plus the 16-bit depth of the wider
/// still-image pipelines today; other codecs may add depths later. Formats whose depth is
/// free-form rather than a fixed set — TIFF and DNG allow any of `1..=16` — carry it as a plain
/// integer instead (`gamut-dng`'s `bits_per_sample`), not as this enum.
#[repr(u8)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BitDepth {
    /// 8 bits per sample.
    Eight = 8,
    /// 10 bits per sample (M2).
    Ten = 10,
    /// 12 bits per sample (M2).
    Twelve = 12,
    /// 16 bits per sample. Not an AV1 profile depth — this is the depth of the 16-bit
    /// interleaved pipelines (PNG/TIFF/JXL/DNG) that share this vocabulary.
    Sixteen = 16,
}

impl BitDepth {
    /// Number of bits per sample.
    #[must_use]
    pub fn bits(self) -> u8 {
        self as u8
    }

    /// The largest sample value a plane of this depth can hold, `(1 << bits) - 1` (255, 1023,
    /// 4095, 65535) — the white level of a full-range plane, and the divisor for normalizing a
    /// sample to `0.0..=1.0`. Returns `u16` because every modeled depth fits one.
    #[must_use]
    pub fn max_value(self) -> u16 {
        // Shifting `u16::MAX` down is exact and total: `1u16 << 16` would overflow at `Sixteen`,
        // and `(1u32 << bits) - 1` would need a truncating cast back to `u16`.
        u16::MAX >> (16 - u32::from(self.bits()))
    }

    /// The [`BitDepth`] for `bits` (8, 10, 12, or 16), or `None` for any other value. The inverse
    /// of [`BitDepth::bits`], for turning a codec's raw integer bit depth back into the typed
    /// form. Takes `u32` because that is what codec headers and reconstruction paths carry;
    /// [`BitDepth::bits`] returns the exact `u8`.
    #[must_use]
    pub fn from_bits(bits: u32) -> Option<Self> {
        match bits {
            8 => Some(BitDepth::Eight),
            10 => Some(BitDepth::Ten),
            12 => Some(BitDepth::Twelve),
            16 => Some(BitDepth::Sixteen),
            _ => None,
        }
    }
}

/// Chroma subsampling of the coded planes (AV1 `subsampling_x` / `subsampling_y`, §5.5.2).
///
/// `#[non_exhaustive]`: models the AV1 layouts and JPEG's 4:4:0 today; other codecs may add
/// layouts (e.g. TIFF's 4:1:1) later.
///
/// `#[repr(u8)]` with explicit, permanent discriminants: this is plain configuration data that
/// must stay mechanically portable to C (see the workspace charter). The values are append-only —
/// a new layout takes the next free discriminant and never renumbers an existing one. They are
/// gamut's own ordering, deliberately *not* a codec field: AVIF's `av1C` mirror
/// (`gamut_avif::ChromaFormat`) carries its own FFI-stable numbering and converts explicitly.
#[repr(u8)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ChromaSubsampling {
    /// 4:4:4 — full-resolution chroma (`subsampling_x = subsampling_y = 0`). Required for identity.
    Cs444 = 0,
    /// 4:2:2 — horizontally halved chroma (M2).
    Cs422 = 1,
    /// 4:2:0 — halved in both directions (M2).
    Cs420 = 2,
    /// 4:0:0 — monochrome, no chroma planes (M2).
    Cs400 = 3,
    /// 4:4:0 — vertically halved chroma at full horizontal resolution: a JPEG sampling-factor
    /// layout (luma `H = 1, V = 2` over `1×1` chroma, T.81 §A.1.1). AV1 cannot signal it
    /// (`subsampling_y = 1` requires `subsampling_x = 1`, §6.4.2), so no AV1 path accepts it.
    Cs440 = 4,
}

impl ChromaSubsampling {
    /// The `(horizontal, vertical)` decimation factors of each chroma plane against luma: how
    /// many luma columns, and how many rows, one chroma sample spans — `(1, 1)` for 4:4:4,
    /// `(2, 1)` for 4:2:2, `(2, 2)` for 4:2:0 and `(1, 2)` for 4:4:0.
    ///
    /// Each factor is 1 or 2, so its base-2 logarithm (`trailing_zeros`) is the AV1
    /// `subsampling_x` / `subsampling_y` flag (§5.5.2) and the sample-position shift.
    /// [`Cs400`](Self::Cs400) has no chroma planes at all; it reports `(2, 2)`, the factors AV1
    /// §6.4.2 infers under `mono_chrome`, which is a header convention rather than a geometry.
    /// Use [`chroma_dimensions`](Self::chroma_dimensions) for plane sizes, which handles that
    /// case.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_color::ChromaSubsampling;
    ///
    /// let (fx, fy) = ChromaSubsampling::Cs422.factors();
    /// assert_eq!((fx, fy), (2, 1));
    /// // The AV1 header flags are the factors' base-2 logarithms.
    /// assert_eq!((fx.trailing_zeros(), fy.trailing_zeros()), (1, 0));
    /// ```
    #[must_use]
    pub fn factors(self) -> (u32, u32) {
        match self {
            ChromaSubsampling::Cs444 => (1, 1),
            ChromaSubsampling::Cs422 => (2, 1),
            ChromaSubsampling::Cs420 | ChromaSubsampling::Cs400 => (2, 2),
            ChromaSubsampling::Cs440 => (1, 2),
        }
    }

    /// The number of coded planes: 1 for [`Cs400`](Self::Cs400) (monochrome), 3 otherwise.
    ///
    /// This is AV1's `NumPlanes` (§5.5.2), the value that gates every chroma syntax element and
    /// every chroma coding step. It is deliberately derived from the layout rather than stored
    /// beside it, so a buffer cannot claim a plane count that disagrees with its subsampling.
    ///
    /// # Examples
    ///
    /// ```
    /// use gamut_color::ChromaSubsampling;
    /// assert_eq!(ChromaSubsampling::Cs444.num_planes(), 3);
    /// assert_eq!(ChromaSubsampling::Cs400.num_planes(), 1);
    /// ```
    #[must_use]
    pub fn num_planes(self) -> usize {
        match self {
            ChromaSubsampling::Cs400 => 1,
            _ => 3,
        }
    }

    /// The dimensions of each chroma (Cb/Cr) plane for a luma plane of `width` × `height`.
    ///
    /// **Ceiling** division on the subsampled axes, so an odd luma dimension keeps the
    /// half-covering edge sample: 4:2:0 ⇒ `(ceil(width / 2), ceil(height / 2))`, 4:2:2 ⇒
    /// `(ceil(width / 2), height)`, 4:4:0 ⇒ `(width, ceil(height / 2))`, 4:4:4 ⇒
    /// `(width, height)`. [`Cs400`](Self::Cs400) has no chroma planes, so it returns `(0, 0)`.
    ///
    /// This is the **visible** geometry. A codec's *coded* chroma plane may be larger, because it
    /// is derived by halving a padded luma grid rather than by rounding the display size up — see
    /// `gamut-av1`'s plane geometry, where the coded halving is exact and only the visible width
    /// needs this rounding.
    #[must_use]
    pub fn chroma_dimensions(self, width: u32, height: u32) -> (u32, u32) {
        match self {
            ChromaSubsampling::Cs444 => (width, height),
            ChromaSubsampling::Cs422 => (width.div_ceil(2), height),
            ChromaSubsampling::Cs420 => (width.div_ceil(2), height.div_ceil(2)),
            ChromaSubsampling::Cs440 => (width, height.div_ceil(2)),
            ChromaSubsampling::Cs400 => (0, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [BitDepth; 4] = [
        BitDepth::Eight,
        BitDepth::Ten,
        BitDepth::Twelve,
        BitDepth::Sixteen,
    ];

    #[test]
    fn bit_depth_bits() {
        assert_eq!(BitDepth::Eight.bits(), 8);
        assert_eq!(BitDepth::Ten.bits(), 10);
        assert_eq!(BitDepth::Twelve.bits(), 12);
        assert_eq!(BitDepth::Sixteen.bits(), 16);
        // from_bits is the inverse of bits() for the four modeled depths; other values are None.
        for d in ALL {
            assert_eq!(BitDepth::from_bits(u32::from(d.bits())), Some(d));
        }
        assert_eq!(BitDepth::from_bits(0), None);
        // 14 is a real RAW sensor depth, deliberately not modeled: depths outside this fixed set
        // are free-form and carried as a plain integer by the format crate (see the enum's docs).
        assert_eq!(BitDepth::from_bits(14), None);
    }

    #[test]
    fn bit_depth_max_value() {
        assert_eq!(BitDepth::Eight.max_value(), 255);
        assert_eq!(BitDepth::Ten.max_value(), 1023);
        assert_eq!(BitDepth::Twelve.max_value(), 4095);
        assert_eq!(BitDepth::Sixteen.max_value(), 65535);
        // Cross-check against the (1 << bits) - 1 definition computed in u32 — a different
        // expression than max_value's own downshift of u16::MAX.
        for d in ALL {
            let expect = (1u32 << d.bits()) - 1;
            assert_eq!(u32::from(d.max_value()), expect);
        }
    }

    #[test]
    fn factors_per_layout() {
        assert_eq!(ChromaSubsampling::Cs444.factors(), (1, 1));
        assert_eq!(ChromaSubsampling::Cs420.factors(), (2, 2));
        assert_eq!(ChromaSubsampling::Cs422.factors(), (2, 1));
        assert_eq!(ChromaSubsampling::Cs440.factors(), (1, 2));
        // Monochrome carries AV1's inferred subsampling_x = subsampling_y = 1 (§6.4.2).
        assert_eq!(ChromaSubsampling::Cs400.factors(), (2, 2));
    }

    #[test]
    fn cs440_has_three_planes() {
        assert_eq!(ChromaSubsampling::Cs440.num_planes(), 3);
    }

    #[test]
    fn chroma_subsampling_discriminants_are_the_frozen_c_abi_values() {
        // Permanent and append-only: a C caller may pass the raw discriminant, so these values are
        // part of the crate's contract and may never be renumbered.
        assert_eq!(ChromaSubsampling::Cs444 as u8, 0);
        assert_eq!(ChromaSubsampling::Cs422 as u8, 1);
        assert_eq!(ChromaSubsampling::Cs420 as u8, 2);
        assert_eq!(ChromaSubsampling::Cs400 as u8, 3);
        assert_eq!(ChromaSubsampling::Cs440 as u8, 4);
    }

    #[test]
    fn chroma_dimensions_round_up_on_the_subsampled_axes() {
        // Each row is exercised on every layout, so a mutant that swaps two arms — or that halves
        // the wrong axis for 4:2:2 — dies on at least one case. Odd dimensions are the point: an
        // odd axis must keep its half-covering edge sample, so `div_ceil`, not `/ 2`.
        let cases = [(1, 1), (1, 2), (2, 1), (3, 3), (5, 7), (16, 16), (17, 13)];
        for (w, h) in cases {
            assert_eq!(
                ChromaSubsampling::Cs444.chroma_dimensions(w, h),
                (w, h),
                "4:4:4 keeps full resolution at {w}x{h}"
            );
            assert_eq!(
                ChromaSubsampling::Cs422.chroma_dimensions(w, h),
                (w.div_ceil(2), h),
                "4:2:2 halves width only at {w}x{h}"
            );
            assert_eq!(
                ChromaSubsampling::Cs420.chroma_dimensions(w, h),
                (w.div_ceil(2), h.div_ceil(2)),
                "4:2:0 halves both axes at {w}x{h}"
            );
            assert_eq!(
                ChromaSubsampling::Cs440.chroma_dimensions(w, h),
                (w, h.div_ceil(2)),
                "4:4:0 halves height only at {w}x{h}"
            );
            assert_eq!(
                ChromaSubsampling::Cs400.chroma_dimensions(w, h),
                (0, 0),
                "monochrome has no chroma planes at {w}x{h}"
            );
        }
        // Pin the ceiling behaviour against literals too, so the assertions above cannot all be
        // satisfied by a mutant that replaced both sides with the same wrong expression.
        assert_eq!(ChromaSubsampling::Cs420.chroma_dimensions(17, 13), (9, 7));
        assert_eq!(ChromaSubsampling::Cs422.chroma_dimensions(17, 13), (9, 13));
        assert_eq!(ChromaSubsampling::Cs440.chroma_dimensions(17, 13), (17, 7));
    }
}
