//! Content sniffing: the one place the CLI decides what container a file is from its leading
//! bytes, never from its extension.
//!
//! `gamut convert` routes its input to a decoder by it, and `gamut inspect` routes a file to a
//! report by it. The signatures are pairwise disjoint, so the order they are tested in decides
//! nothing.

/// A container recognised by its signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Container {
    /// The 8-byte PNG signature (PNG 3rd edition §5.2).
    Png,
    /// The JPEG SOI marker followed by the next marker's `FF` (T.81 §B.1.1.3).
    Jpeg,
    /// A RIFF container whose form type is `WEBP` (RFC 9649 §2.4).
    Webp,
    /// A JPEG XL bare codestream (`FF 0A`) or its ISO BMFF container's leading `JXL ` box
    /// (ISO/IEC 18181-2 §9.1).
    Jxl,
    /// An ISO BMFF file whose first box is `ftyp` (ISO/IEC 14496-12 §4.3) — a route to the HEIF
    /// still-image formats, not yet a verdict on which one.
    Isobmff,
}

/// The PNG file signature.
const PNG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The JPEG XL ISO BMFF container signature: a 12-byte `JXL ` box.
const JXL_CONTAINER: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
];

/// The container `bytes` begins with, or `None` when it begins with none of them.
///
/// A JPEG XL container's first box is `JXL `, not `ftyp`, so it is [`Container::Jxl`] and never
/// [`Container::Isobmff`].
pub(crate) fn sniff(bytes: &[u8]) -> Option<Container> {
    if bytes.starts_with(&PNG) {
        Some(Container::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Container::Jpeg)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(Container::Webp)
    } else if bytes.starts_with(&[0xFF, 0x0A]) || bytes.starts_with(&JXL_CONTAINER) {
        Some(Container::Jxl)
    } else if bytes.get(4..8) == Some(b"ftyp") {
        Some(Container::Isobmff)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Container, sniff};

    #[test]
    fn each_signature_names_its_container() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n"), Some(Container::Png));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Container::Jpeg));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8L"), Some(Container::Webp));
        assert_eq!(sniff(&[0xFF, 0x0A, 0x00]), Some(Container::Jxl));
        assert_eq!(
            sniff(b"\0\0\0\x0cJXL \r\n\x87\n\0\0\0\x14ftyp"),
            Some(Container::Jxl)
        );
        assert_eq!(sniff(b"\0\0\0\x18ftypheic"), Some(Container::Isobmff));
    }

    #[test]
    fn a_near_miss_names_no_container() {
        // One byte short of, or one byte off, each signature.
        assert_eq!(sniff(b"\x89PNG\r\n\x1a"), None);
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFE]), None);
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBQ"), None);
        assert_eq!(sniff(b"RIFF\0\0\0\0WEB"), None);
        assert_eq!(sniff(b"RIFX\0\0\0\0WEBP"), None);
        assert_eq!(sniff(&[0xFF, 0x0B]), None);
        assert_eq!(sniff(b"\0\0\0\x0cJXL \r\n\x87\x0b"), None);
        assert_eq!(sniff(b"\0\0\0\x18ftyq"), None);
        // TIFF has no signature here: `gamut inspect` decides TIFF and DNG by walking the IFDs.
        assert_eq!(sniff(b"II*\0\x08\0\0\0"), None);
        assert_eq!(sniff(&[]), None);
    }
}
