//! [`Cicp`]: the ITU-T H.273 coding-independent code points every format signals colour with.

/// The four ITU-T H.273 (ISO/IEC 23091-2) code points identifying a video signal type: colour
/// primaries, transfer characteristics, matrix coefficients, and the full-range flag.
///
/// One representation for every carrier — an AVIF/HEIF `colr` box of type `nclx`, an ICC
/// `cicpType` element, a PNG `cICP` chunk, an AV1 sequence header. The code points are `u16` so a
/// reader keeps whatever value a file carries, including ones H.273 reserves; mapping them to
/// concrete primaries and transfer functions is `gamut-color`'s job, not this type's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cicp {
    /// `ColourPrimaries` (H.273 §8.1).
    pub colour_primaries: u16,
    /// `TransferCharacteristics` (H.273 §8.2).
    pub transfer_characteristics: u16,
    /// `MatrixCoefficients` (H.273 §8.3).
    pub matrix_coefficients: u16,
    /// `VideoFullRangeFlag` (H.273 §8.3): `true` for full range, `false` for narrow (studio) range.
    pub full_range: bool,
}
