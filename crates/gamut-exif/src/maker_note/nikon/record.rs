//! A `Nikon3` note's version-gated records, kept opaque (issue #979's decision).

use gamut_ifd::Ifd;

use super::{COLOR_BALANCE, LENS_DATA, SHOT_INFO, undefined};

/// Which of a `Nikon3` note's version-gated records a [`NikonRecord`] is.
///
/// `#[repr(u8)]` with permanent, append-only discriminants, so the value crosses a C boundary
/// unchanged.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NikonRecordKind {
    /// `ShotInfo` (`0x0091`).
    ShotInfo = 0,
    /// `ColorBalance` (`0x0097`).
    ColorBalance = 1,
    /// `LensData` (`0x0098`).
    LensData = 2,
}

impl NikonRecordKind {
    /// The record's tag in the note's IFD.
    #[must_use]
    pub fn tag(self) -> u16 {
        match self {
            NikonRecordKind::ShotInfo => SHOT_INFO,
            NikonRecordKind::ColorBalance => COLOR_BALANCE,
            NikonRecordKind::LensData => LENS_DATA,
        }
    }
}

/// One of a `Nikon3` note's version-gated records (`ShotInfo`, `ColorBalance`, `LensData`),
/// **kept opaque**: the record it is, its four version bytes, and the whole value verbatim.
///
/// The first four bytes of each record are an ASCII version (`"0100"`, `"0204"`, …) that decides
/// the layout of the rest — and, for most versions, that the rest is enciphered, keyed by the
/// note's `SerialNumber` and `ShutterCount` (exiv2's `nikonCrypt`; the versions are tabled in
/// `references/exif/nikon/README.md`). This view deciphers nothing and interprets nothing past the
/// version (issue #1283 tracks deciphering them); [`bytes`](Self::bytes) is exactly what the note
/// stores, so the record re-emits byte-identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NikonRecord<'a> {
    kind: NikonRecordKind,
    version: [u8; 4],
    bytes: &'a [u8],
}

impl<'a> NikonRecord<'a> {
    /// `kind`'s record in `ifd`: an `UNDEFINED` value of at least its four version bytes;
    /// anything else is no record. The length floor is exiv2's (`nikonSelector` refuses a value
    /// under four bytes); the type check is gamut's own, since exiv2 reads the raw bytes of any
    /// type — its tag table types each record `UNDEFINED`, and only that type's value is the
    /// stored bytes as such, independent of byte order.
    pub(super) fn read(ifd: &'a Ifd, kind: NikonRecordKind) -> Option<Self> {
        let bytes = undefined(ifd, kind.tag())?;
        let version = bytes.first_chunk::<4>()?;
        Some(Self {
            kind,
            version: *version,
            bytes,
        })
    }

    /// The record this is.
    #[must_use]
    pub fn kind(&self) -> NikonRecordKind {
        self.kind
    }

    /// The record's version: its first four bytes, as stored (ASCII digits in every body exiv2
    /// knows, e.g. `*b"0204"`).
    #[must_use]
    pub fn version(&self) -> [u8; 4] {
        self.version
    }

    /// The whole value verbatim, the version included and anything enciphered still enciphered.
    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use gamut_ifd::Value;

    use super::*;

    #[test]
    fn record_kinds_name_their_tags_and_frozen_discriminants() {
        let kinds = [
            (NikonRecordKind::ShotInfo, 0x0091, 0),
            (NikonRecordKind::ColorBalance, 0x0097, 1),
            (NikonRecordKind::LensData, 0x0098, 2),
        ];
        for (kind, tag, discriminant) in kinds {
            assert_eq!(kind.tag(), tag, "{kind:?}");
            assert_eq!(kind as u8, discriminant, "{kind:?}");
        }
    }

    /// Exactly the four version bytes is a record; one byte fewer, another type, or no entry at
    /// all is none (exiv2's selector refuses the short one too).
    #[test]
    fn a_record_needs_its_four_version_bytes_as_undefined() {
        let mut ifd = Ifd::new();
        ifd.set(SHOT_INFO, Value::Undefined(b"0204".to_vec()));
        ifd.set(COLOR_BALANCE, Value::Undefined(b"010".to_vec()));
        ifd.set(LENS_DATA, Value::Byte(b"0101 data".to_vec()));
        let shot = NikonRecord::read(&ifd, NikonRecordKind::ShotInfo).expect("a record");
        assert_eq!(shot.version(), *b"0204");
        assert_eq!(shot.bytes(), b"0204");
        assert_eq!(NikonRecord::read(&ifd, NikonRecordKind::ColorBalance), None);
        assert_eq!(NikonRecord::read(&ifd, NikonRecordKind::LensData), None);
        assert_eq!(
            NikonRecord::read(&Ifd::new(), NikonRecordKind::ShotInfo),
            None
        );
    }
}
