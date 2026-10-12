//! CICP (ITU-T H.273 / ISO/IEC 23091-2) code points shared by the AVIF `colr` (nclx) box and the
//! AV1 sequence header `color_config` (AV1 §5.5.2 / §6.4.2).
//!
//! Each enum is `#[repr(u16)]` with discriminants equal to the spec code points, so
//! [`MatrixCoefficients::code_point`] (and the siblings) is just the discriminant. M0 uses
//! `MatrixCoefficients::Identity`, `ColourPrimaries::Bt709`, `TransferCharacteristics::Srgb`, and
//! `ColorRange::Full`; the remaining named values are included for M2/M4 extension.
//!
//! Naming follows each identifier's source spec: the H.273 types keep the spec's British
//! spelling ([`ColourPrimaries`], as published), while AV1-derived names use the AV1 spec's
//! American spelling ([`ColorRange`], after `color_range`).

/// Matrix coefficients (CICP `MatrixCoefficients`). `Identity` (0) carries RGB directly with no
/// colour transform and requires 4:4:4 — the basis for lossless RGB AVIF.
///
/// `#[non_exhaustive]`: H.273 defines further code points, added here as milestones need them.
#[repr(u16)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MatrixCoefficients {
    /// Identity (RGB / "GBR"); no luma–chroma transform. Requires 4:4:4. (Code point 0.)
    Identity = 0,
    /// BT.709 — KR=0.2126, KB=0.0722. (Code point 1.)
    Bt709 = 1,
    /// Unspecified. (Code point 2.)
    Unspecified = 2,
    /// BT.470 System B,G — KR=0.299, KB=0.114, identical to [`Bt601`](Self::Bt601).
    /// (Code point 5.)
    ///
    /// A distinct code point naming the same de-matrixing: to test whether two streams convert
    /// alike, compare the [`YcbcrMatrix`](crate::YcbcrMatrix) values, not the
    /// [`MatrixCoefficients`] values. Kept distinct so a `colr` box read as 5 is written back as 5.
    Bt470Bg = 5,
    /// BT.601 / SMPTE 170M — KR=0.299, KB=0.114. (Code point 6; [`Bt470Bg`](Self::Bt470Bg) is
    /// point 5, with identical coefficients.)
    Bt601 = 6,
    /// YCgCo. (Code point 8.)
    YCgCo = 8,
    /// BT.2020 non-constant luminance. (Code point 9.)
    Bt2020Ncl = 9,
}

impl MatrixCoefficients {
    /// The CICP code point.
    #[must_use]
    pub fn code_point(self) -> u16 {
        self as u16
    }

    /// The [`MatrixCoefficients`] for `code_point`, or `None` for any point not modeled here (a
    /// later minor release may turn a `None` into `Some`). The inverse of
    /// [`MatrixCoefficients::code_point`], for typing a value parsed from a `colr` box or AV1
    /// header.
    #[must_use]
    pub fn from_code_point(code_point: u16) -> Option<Self> {
        match code_point {
            0 => Some(MatrixCoefficients::Identity),
            1 => Some(MatrixCoefficients::Bt709),
            2 => Some(MatrixCoefficients::Unspecified),
            5 => Some(MatrixCoefficients::Bt470Bg),
            6 => Some(MatrixCoefficients::Bt601),
            8 => Some(MatrixCoefficients::YCgCo),
            9 => Some(MatrixCoefficients::Bt2020Ncl),
            _ => None,
        }
    }
}

/// Colour primaries (CICP `ColourPrimaries`).
///
/// `#[non_exhaustive]`: H.273 defines further code points, added here as milestones need them.
#[doc(alias = "ColorPrimaries")]
#[repr(u16)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ColourPrimaries {
    /// BT.709 (also sRGB primaries). (Code point 1.)
    Bt709 = 1,
    /// Unspecified. (Code point 2.)
    Unspecified = 2,
    /// BT.601 625-line / BT.470 System B,G. (Code point 5.)
    Bt601Pal = 5,
    /// SMPTE 170M (BT.601 525-line). (Code point 6.)
    Smpte170m = 6,
    /// BT.2020 / BT.2100. (Code point 9.)
    Bt2020 = 9,
    /// SMPTE EG 432-1 (Display P3). (Code point 12.)
    DisplayP3 = 12,
}

impl ColourPrimaries {
    /// The CICP code point.
    #[must_use]
    pub fn code_point(self) -> u16 {
        self as u16
    }

    /// The [`ColourPrimaries`] for `code_point`, or `None` for any point not modeled here (a
    /// later minor release may turn a `None` into `Some`). The inverse of
    /// [`ColourPrimaries::code_point`].
    #[must_use]
    pub fn from_code_point(code_point: u16) -> Option<Self> {
        match code_point {
            1 => Some(ColourPrimaries::Bt709),
            2 => Some(ColourPrimaries::Unspecified),
            5 => Some(ColourPrimaries::Bt601Pal),
            6 => Some(ColourPrimaries::Smpte170m),
            9 => Some(ColourPrimaries::Bt2020),
            12 => Some(ColourPrimaries::DisplayP3),
            _ => None,
        }
    }

    /// The CIE 1931 chromaticities this code point names: the `[R, G, B]` primaries as `(x, y)`
    /// pairs, and the white point, exactly as [`crate::matrix::rgb_to_xyz_matrix`] takes them.
    ///
    /// `None` for [`ColourPrimaries::Unspecified`], which by definition names no chromaticities
    /// (a later minor release may turn a `None` into `Some` as further code points are modeled).
    ///
    /// Values are ITU-T H.273 Table 2. Note that [`ColourPrimaries::Bt601Pal`] (625-line, EBU
    /// Tech. 3213-E) and [`ColourPrimaries::Smpte170m`] (525-line) are **different** primary
    /// sets, unlike the [`MatrixCoefficients::Bt470Bg`]/[`MatrixCoefficients::Bt601`] pair, whose
    /// coefficients are identical.
    ///
    /// # Example
    ///
    /// Build the linear-RGB → XYZ matrix for a CICP-tagged image:
    ///
    /// ```
    /// use gamut_color::cicp::ColourPrimaries;
    /// use gamut_color::matrix::rgb_to_xyz_matrix;
    ///
    /// let (primaries, white) = ColourPrimaries::Bt2020.chromaticities().unwrap();
    /// let m = rgb_to_xyz_matrix(&primaries, white).unwrap();
    /// // Row 1 of an RGB→XYZ matrix is the luminance (Y) weighting, so it sums to 1.
    /// assert!((m[1].iter().sum::<f64>() - 1.0).abs() < 1e-12);
    ///
    /// assert_eq!(ColourPrimaries::Unspecified.chromaticities(), None);
    /// ```
    #[must_use]
    pub fn chromaticities(self) -> Option<([[f64; 2]; 3], [f64; 2])> {
        use crate::matrix::{
            BT601_525_PRIMARIES, BT601_625_PRIMARIES, BT2020_PRIMARIES, D65, DISPLAY_P3_PRIMARIES,
            SRGB_PRIMARIES,
        };
        match self {
            // BT.709 and sRGB share one set of primaries.
            ColourPrimaries::Bt709 => Some((SRGB_PRIMARIES, D65)),
            ColourPrimaries::Bt601Pal => Some((BT601_625_PRIMARIES, D65)),
            ColourPrimaries::Smpte170m => Some((BT601_525_PRIMARIES, D65)),
            ColourPrimaries::Bt2020 => Some((BT2020_PRIMARIES, D65)),
            ColourPrimaries::DisplayP3 => Some((DISPLAY_P3_PRIMARIES, D65)),
            ColourPrimaries::Unspecified => None,
        }
    }
}

/// Transfer characteristics (CICP `TransferCharacteristics`).
///
/// `#[non_exhaustive]`: H.273 defines further code points, added here as milestones need them.
#[repr(u16)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TransferCharacteristics {
    /// BT.709. (Code point 1.)
    Bt709 = 1,
    /// Unspecified. (Code point 2.)
    Unspecified = 2,
    /// BT.601 / SMPTE 170M. (Code point 6.) ITU-T H.273 Table 3 gives it the same curve as
    /// [`Bt709`](Self::Bt709), [`Bt2020_10`](Self::Bt2020_10) and [`Bt2020_12`](Self::Bt2020_12);
    /// kept distinct so a `colr` box read as 6 is written back as 6.
    Bt601 = 6,
    /// Linear — the identity transfer, `V = Lc`. (Code point 8.)
    Linear = 8,
    /// sRGB / IEC 61966-2-1. (Code point 13.)
    Srgb = 13,
    /// BT.2020 10-bit. (Code point 14.)
    Bt2020_10 = 14,
    /// BT.2020 12-bit. (Code point 15.) ITU-T H.273 Table 3: "functionally the same as the
    /// values 1, 6 and 14".
    Bt2020_12 = 15,
    /// SMPTE ST 2084 (PQ). (Code point 16.)
    Pq = 16,
    /// ARIB STD-B67 (HLG). (Code point 18.)
    Hlg = 18,
}

impl TransferCharacteristics {
    /// The CICP code point.
    #[must_use]
    pub fn code_point(self) -> u16 {
        self as u16
    }

    /// The [`TransferCharacteristics`] for `code_point`, or `None` for any point not modeled here
    /// (a later minor release may turn a `None` into `Some`). The inverse of
    /// [`TransferCharacteristics::code_point`].
    #[must_use]
    pub fn from_code_point(code_point: u16) -> Option<Self> {
        match code_point {
            1 => Some(TransferCharacteristics::Bt709),
            2 => Some(TransferCharacteristics::Unspecified),
            6 => Some(TransferCharacteristics::Bt601),
            8 => Some(TransferCharacteristics::Linear),
            13 => Some(TransferCharacteristics::Srgb),
            14 => Some(TransferCharacteristics::Bt2020_10),
            15 => Some(TransferCharacteristics::Bt2020_12),
            16 => Some(TransferCharacteristics::Pq),
            18 => Some(TransferCharacteristics::Hlg),
            _ => None,
        }
    }
}

/// Sample value range (CICP `VideoFullRangeFlag`; AV1 `color_range`).
///
/// Besides signalling the range in a `colr` / AV1 header, this is the range selector for the
/// RGB ↔ YCbCr conversions in [`crate::ycbcr`] — so one type carries the choice end to end.
///
/// Deliberately exhaustive (unlike the code-point enums): the flag is a spec-complete single bit,
/// so consumers can match it exhaustively forever.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ColorRange {
    /// Studio / limited range (e.g. luma 16–235 for 8-bit). (Flag 0.)
    Limited = 0,
    /// Full range (0–255 for 8-bit). (Flag 1.)
    Full = 1,
}

impl ColorRange {
    /// The `color_range` / `full_range_flag` value.
    #[must_use]
    pub fn flag(self) -> u8 {
        self as u8
    }

    /// The [`ColorRange`] for `flag` (0 or 1), or `None` for any other value. The inverse of
    /// [`ColorRange::flag`].
    #[must_use]
    pub fn from_flag(flag: u8) -> Option<Self> {
        match flag {
            0 => Some(ColorRange::Limited),
            1 => Some(ColorRange::Full),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_points_match_spec() {
        assert_eq!(MatrixCoefficients::Identity.code_point(), 0);
        assert_eq!(MatrixCoefficients::Bt709.code_point(), 1);
        assert_eq!(MatrixCoefficients::Bt470Bg.code_point(), 5);
        assert_eq!(MatrixCoefficients::Bt601.code_point(), 6);
        assert_eq!(MatrixCoefficients::Bt2020Ncl.code_point(), 9);
        assert_eq!(ColourPrimaries::Bt709.code_point(), 1);
        // A second, non-1 primaries value pins `self as u16` (a constant `1` would pass on Bt709
        // alone).
        assert_eq!(ColourPrimaries::Bt2020.code_point(), 9);
        assert_eq!(ColourPrimaries::DisplayP3.code_point(), 12);
        assert_eq!(TransferCharacteristics::Linear.code_point(), 8);
        assert_eq!(TransferCharacteristics::Srgb.code_point(), 13);
        assert_eq!(TransferCharacteristics::Pq.code_point(), 16);
        assert_eq!(ColorRange::Full.flag(), 1);
        assert_eq!(ColorRange::Limited.flag(), 0);
    }

    #[test]
    fn code_point_inverses_round_trip() {
        use ColourPrimaries as Cp;
        use MatrixCoefficients as Mc;
        use TransferCharacteristics as Tc;
        for mc in [
            Mc::Identity,
            Mc::Bt709,
            Mc::Unspecified,
            Mc::Bt470Bg,
            Mc::Bt601,
            Mc::YCgCo,
            Mc::Bt2020Ncl,
        ] {
            assert_eq!(Mc::from_code_point(mc.code_point()), Some(mc));
        }
        for cp in [
            Cp::Bt709,
            Cp::Unspecified,
            Cp::Bt601Pal,
            Cp::Smpte170m,
            Cp::Bt2020,
            Cp::DisplayP3,
        ] {
            assert_eq!(Cp::from_code_point(cp.code_point()), Some(cp));
        }
        for tc in [
            Tc::Bt709,
            Tc::Unspecified,
            Tc::Bt601,
            Tc::Linear,
            Tc::Srgb,
            Tc::Bt2020_10,
            Tc::Bt2020_12,
            Tc::Pq,
            Tc::Hlg,
        ] {
            assert_eq!(Tc::from_code_point(tc.code_point()), Some(tc));
        }
        for range in [ColorRange::Limited, ColorRange::Full] {
            assert_eq!(ColorRange::from_flag(range.flag()), Some(range));
        }
        // Unmodeled points map to None (3 = "reserved" / not modeled in every table).
        assert_eq!(Mc::from_code_point(3), None);
        assert_eq!(Cp::from_code_point(3), None);
        assert_eq!(Tc::from_code_point(3), None);
        assert_eq!(ColorRange::from_flag(2), None);
    }

    /// The H.273 Table 2 chromaticities, asserted as published literals rather than by
    /// round-tripping through the crate's own tables — a wrong constant is exactly the failure
    /// mode here, and checking `matrix.rs`'s consts against `matrix.rs`'s consts proves nothing.
    #[test]
    fn chromaticities_match_h273_table_2() {
        let (p, w) = ColourPrimaries::Bt709
            .chromaticities()
            .expect("BT.709 is modeled");
        assert_eq!(p, [[0.640, 0.330], [0.300, 0.600], [0.150, 0.060]]);
        assert_eq!(w, [0.3127, 0.3290]);

        let (p, _) = ColourPrimaries::Bt601Pal
            .chromaticities()
            .expect("625-line is modeled");
        assert_eq!(p, [[0.640, 0.330], [0.290, 0.600], [0.150, 0.060]]);

        let (p, _) = ColourPrimaries::Smpte170m
            .chromaticities()
            .expect("525-line is modeled");
        assert_eq!(p, [[0.630, 0.340], [0.310, 0.595], [0.155, 0.070]]);

        let (p, _) = ColourPrimaries::Bt2020
            .chromaticities()
            .expect("BT.2020 is modeled");
        assert_eq!(p, [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046]]);

        let (p, _) = ColourPrimaries::DisplayP3
            .chromaticities()
            .expect("P3 is modeled");
        assert_eq!(p, [[0.680, 0.320], [0.265, 0.690], [0.150, 0.060]]);

        // Every modeled point is D65; a mutant substituting D50 would otherwise survive on the
        // four variants whose white point is not spelled out above.
        for cp in MODELED_PRIMARIES {
            assert_eq!(cp.chromaticities().expect("modeled").1, [0.3127, 0.3290]);
        }

        // "Unspecified" names no chromaticities by definition.
        assert_eq!(ColourPrimaries::Unspecified.chromaticities(), None);
    }

    /// The two "BT.601" primary sets are genuinely different — the one distinction in this table
    /// that is easy to collapse by mistake, since the *matrix* coefficients for code points 5 and
    /// 6 really are identical (see `MatrixCoefficients::Bt470Bg`/`Bt601`) and invite the same
    /// assumption here. Compare the derived matrices, not just the constants, so the difference
    /// is shown to be observable downstream.
    #[test]
    fn bt601_625_and_525_are_distinct_primaries() {
        let (p625, w) = ColourPrimaries::Bt601Pal.chromaticities().expect("modeled");
        let (p525, _) = ColourPrimaries::Smpte170m
            .chromaticities()
            .expect("modeled");
        assert_ne!(p625, p525);

        let m625 = crate::matrix::rgb_to_xyz_matrix(&p625, w).expect("non-degenerate");
        let m525 = crate::matrix::rgb_to_xyz_matrix(&p525, w).expect("non-degenerate");
        // Red's X contribution differs by ~0.01 — orders of magnitude above rounding.
        assert!((m625[0][0] - m525[0][0]).abs() > 1e-3);
    }

    /// Composition check: the table is not merely present, it is wired to reproduce the published
    /// sRGB→XYZ matrix. Gate matches `matrix.rs`'s own Lindbloom comparison (5e-4).
    #[test]
    fn bt709_chromaticities_derive_the_published_srgb_matrix() {
        let (primaries, white) = ColourPrimaries::Bt709.chromaticities().expect("modeled");
        let m = crate::matrix::rgb_to_xyz_matrix(&primaries, white).expect("non-degenerate");
        // Bruce Lindbloom's published sRGB (D65) RGB→XYZ matrix.
        let expected = [
            [0.412_456_4, 0.357_576_1, 0.180_437_5],
            [0.212_672_9, 0.715_152_2, 0.072_175_0],
            [0.019_333_9, 0.119_192_0, 0.950_304_1],
        ];
        for (row, exp_row) in m.iter().zip(expected.iter()) {
            for (got, exp) in row.iter().zip(exp_row.iter()) {
                assert!((got - exp).abs() < 5e-4, "got {got}, expected {exp}");
            }
        }
    }

    /// Every modeled code point yields non-degenerate primaries, so composing `chromaticities`
    /// with `rgb_to_xyz_matrix` never hands a caller a `None` it cannot act on.
    #[test]
    fn every_modeled_primary_set_builds_a_matrix() {
        for cp in MODELED_PRIMARIES {
            let (p, w) = cp.chromaticities().expect("modeled");
            let m = crate::matrix::rgb_to_xyz_matrix(&p, w).expect("non-degenerate");
            // Row 1 is the luminance weighting: by construction it sums to 1 at the white point.
            let luma: f64 = m[1].iter().sum();
            assert!((luma - 1.0).abs() < 1e-12, "{cp:?} luma row sums to {luma}");
        }
    }

    /// `Unspecified` is the *only* modeled point without chromaticities. A new variant added
    /// without a table entry fails to compile (the match is exhaustive); this pins the other
    /// half, that no modeled variant silently returns `None`.
    #[test]
    fn only_unspecified_lacks_chromaticities() {
        for code in 0..=u16::from(u8::MAX) {
            if let Some(cp) = ColourPrimaries::from_code_point(code) {
                assert_eq!(
                    cp.chromaticities().is_none(),
                    cp == ColourPrimaries::Unspecified,
                    "{cp:?} disagrees with the Unspecified-only rule"
                );
            }
        }
    }

    /// Every `ColourPrimaries` that names real chromaticities.
    const MODELED_PRIMARIES: [ColourPrimaries; 5] = [
        ColourPrimaries::Bt709,
        ColourPrimaries::Bt601Pal,
        ColourPrimaries::Smpte170m,
        ColourPrimaries::Bt2020,
        ColourPrimaries::DisplayP3,
    ];
}
