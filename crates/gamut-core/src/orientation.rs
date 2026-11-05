//! [`Orientation`]: the eight EXIF display orientations.

/// The display orientation of the coded samples, one variant per EXIF `Orientation` value
/// (CIPA DC-008, tag `0x0112`); the discriminant *is* that value.
///
/// Orientation is metadata: an encoder stores the caller's samples in coded order and a viewer
/// applies the transform on display. The four transposing variants ([`Orientation::Transpose`],
/// [`Orientation::Rotate90Cw`], [`Orientation::AntiTranspose`], [`Orientation::Rotate90Ccw`]) swap
/// the displayed width and height relative to the coded dimensions. The default is
/// [`Orientation::Identity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Orientation {
    /// No transform (EXIF 1); the default.
    #[default]
    Identity = 1,
    /// Mirror horizontally (EXIF 2).
    FlipHorizontal = 2,
    /// Rotate 180 degrees (EXIF 3).
    Rotate180 = 3,
    /// Mirror vertically (EXIF 4).
    FlipVertical = 4,
    /// Transpose: mirror across the main diagonal (EXIF 5). Swaps displayed width/height.
    Transpose = 5,
    /// Rotate 90 degrees clockwise (EXIF 6). Swaps displayed width/height.
    Rotate90Cw = 6,
    /// Anti-transpose: mirror across the anti-diagonal (EXIF 7). Swaps displayed width/height.
    AntiTranspose = 7,
    /// Rotate 90 degrees counter-clockwise (EXIF 8). Swaps displayed width/height.
    Rotate90Ccw = 8,
}

impl Orientation {
    /// The EXIF orientation value (`1..=8`) this variant signals.
    #[must_use]
    pub const fn exif_value(self) -> u8 {
        self as u8
    }

    /// The [`Orientation`] for an EXIF orientation value, or `None` if `value` is outside `1..=8`.
    ///
    /// The inverse of [`Orientation::exif_value`].
    #[must_use]
    pub const fn from_exif_value(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Identity,
            2 => Self::FlipHorizontal,
            3 => Self::Rotate180,
            4 => Self::FlipVertical,
            5 => Self::Transpose,
            6 => Self::Rotate90Cw,
            7 => Self::AntiTranspose,
            8 => Self::Rotate90Ccw,
            _ => return None,
        })
    }

    /// Whether this orientation swaps the displayed width and height relative to the coded
    /// dimensions (the four diagonal and rotating transforms, EXIF `5..=8`).
    #[must_use]
    pub const fn transposes(self) -> bool {
        self as u8 >= Self::Transpose as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Orientation; 8] = [
        Orientation::Identity,
        Orientation::FlipHorizontal,
        Orientation::Rotate180,
        Orientation::FlipVertical,
        Orientation::Transpose,
        Orientation::Rotate90Cw,
        Orientation::AntiTranspose,
        Orientation::Rotate90Ccw,
    ];

    #[test]
    fn exif_values_are_one_through_eight_in_order() {
        for (value, orientation) in (1u8..=8).zip(ALL) {
            assert_eq!(orientation.exif_value(), value);
        }
        assert_eq!(Orientation::default(), Orientation::Identity);
    }

    #[test]
    fn from_exif_value_inverts_exif_value_and_rejects_the_rest() {
        for orientation in ALL {
            assert_eq!(
                Orientation::from_exif_value(orientation.exif_value()),
                Some(orientation)
            );
        }
        assert_eq!(Orientation::from_exif_value(0), None);
        assert_eq!(Orientation::from_exif_value(9), None);
    }

    #[test]
    fn only_exif_five_through_eight_transpose() {
        for orientation in ALL {
            assert_eq!(
                orientation.transposes(),
                orientation.exif_value() >= 5,
                "{orientation:?}"
            );
        }
    }
}
