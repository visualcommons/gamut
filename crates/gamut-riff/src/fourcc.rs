//! Four-character codes (FourCC) — the 4-byte ASCII tags that identify RIFF forms and chunks
//! (RFC 9649 §2.2; Google *WebP Container*, "Terminology & Basics").

use core::fmt;

/// A four-character code: the 4-byte ASCII tag identifying a RIFF form or chunk.
///
/// FourCCs are compared by their raw bytes, so `VP8 ` (with a trailing space, `0x20`) and `VP8L`
/// are distinct codes — as required by the WebP container spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCc(pub [u8; 4]);

impl FourCc {
    /// RIFF container magic (`RIFF`).
    pub const RIFF: Self = Self(*b"RIFF");
    /// WebP form type (`WEBP`), following the `RIFF` magic and file size.
    pub const WEBP: Self = Self(*b"WEBP");
    /// Lossy VP8 bitstream chunk (`VP8 ` — note the trailing space, `0x20`).
    pub const VP8: Self = Self(*b"VP8 ");
    /// Lossless VP8L bitstream chunk (`VP8L`).
    pub const VP8L: Self = Self(*b"VP8L");
    /// Extended-format feature header chunk (`VP8X`).
    pub const VP8X: Self = Self(*b"VP8X");
    /// Alpha bitstream chunk (`ALPH`).
    pub const ALPH: Self = Self(*b"ALPH");
    /// ICC color-profile chunk (`ICCP`).
    pub const ICCP: Self = Self(*b"ICCP");
    /// Exif metadata chunk (`EXIF`).
    pub const EXIF: Self = Self(*b"EXIF");
    /// XMP metadata chunk (`XMP ` — note the trailing space, `0x20`).
    pub const XMP: Self = Self(*b"XMP ");
    /// Global animation-parameters chunk (`ANIM`).
    pub const ANIM: Self = Self(*b"ANIM");
    /// Per-frame animation chunk (`ANMF`).
    pub const ANMF: Self = Self(*b"ANMF");
    /// C2PA manifest-store chunk (`C2PA`).
    ///
    /// The one code here that RFC 9649 does not define: it is C2PA 2.4 §A.3.7's, which places the
    /// chunk last in the `RIFF`/`WEBP` form. To the WebP container itself it is an unknown chunk
    /// readers ignore (§2.7.1.6).
    pub const C2PA: Self = Self(*b"C2PA");

    /// Returns the raw four bytes of the code.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 4] {
        &self.0
    }
}

impl From<[u8; 4]> for FourCc {
    fn from(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for FourCc {
    /// Renders the code as ASCII, escaping any non-printable byte as `\xHH` so the output stays a
    /// single readable line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            if b.is_ascii_graphic() || b == b' ' {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_bytes_and_from_roundtrip() {
        let fc = FourCc::from(*b"VP8L");
        assert_eq!(fc.as_bytes(), b"VP8L");
        assert_eq!(fc, FourCc::VP8L);
    }

    #[test]
    fn vp8_space_is_distinct_from_vp8l() {
        assert_ne!(FourCc::VP8, FourCc::VP8L);
        assert_eq!(FourCc::VP8.as_bytes(), b"VP8 ");
    }

    #[test]
    fn display_renders_ascii_and_escapes_controls() {
        assert_eq!(FourCc::VP8.to_string(), "VP8 ");
        assert_eq!(FourCc::WEBP.to_string(), "WEBP");
        assert_eq!(
            FourCc::from([0x00, b'A', 0x7f, b'Z']).to_string(),
            "\\x00A\\x7fZ"
        );
    }
}
