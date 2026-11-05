//! The 128-byte ICC profile header (ICC.1:2022 §7.2).

use md5::{Digest, Md5};

use crate::bytes::{ByteReader, push_date_time, push_xyz_number};
use crate::error::{IccError, Result};
use crate::primitives::{DateTime, Signature, XyzNumber};

/// The fixed 128-byte header that opens every ICC profile (ICC.1:2022 §7.2).
///
/// Records the profile's size, the device/connection colour spaces it relates, the version,
/// the default rendering intent, the PCS illuminant, and an MD5 identifier. Every field is modelled
/// (open-registry signatures kept raw, closed registries as enums) so a parse → serialize round-trip
/// is lossless; the 4-byte `acsp` magic at offset 36 is validated on read and re-emitted on write,
/// so it is not a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileHeader {
    /// Total profile size in bytes (offset 0). Recomputed by the writer.
    pub size: u32,
    /// Preferred CMM signature (offset 4, e.g. `appl`), or [`Signature::ZERO`].
    pub preferred_cmm: Signature,
    /// Profile format version (offset 8).
    pub version: ProfileVersion,
    /// Device/profile class (offset 12).
    pub device_class: DeviceClass,
    /// The data (device, "A"-side) colour space (offset 16).
    pub data_color_space: ColorSpace,
    /// The profile connection space (offset 20). Usually `XYZ`/`Lab`, but a device link records
    /// its output device space here.
    pub pcs: ColorSpace,
    /// Profile creation date-time in UTC (offset 24); may be [`DateTime::ZERO`].
    pub created: DateTime,
    /// Primary platform signature (offset 40, e.g. `APPL`/`MSFT`), or [`Signature::ZERO`].
    pub platform: Signature,
    /// Profile flags (offset 44): bit 0 = embedded, bit 1 = cannot be used independently; the
    /// upper half is CMM-private. Stored raw; decode the ICC-defined bits with
    /// [`ProfileHeader::is_embedded`] and [`ProfileHeader::cannot_be_used_independently`].
    pub flags: u32,
    /// Device manufacturer signature (offset 48), or [`Signature::ZERO`].
    pub manufacturer: Signature,
    /// Device model signature (offset 52), or [`Signature::ZERO`].
    pub model: Signature,
    /// Device attributes (offset 56): reflective/transparency, glossy/matte, polarity, colour/BW
    /// in the low bits; the upper half is vendor-specific. Stored raw; decode the ICC-defined bits
    /// with [`ProfileHeader::is_transparency`], [`ProfileHeader::is_matte`],
    /// [`ProfileHeader::is_negative_polarity`], and [`ProfileHeader::is_black_and_white`].
    pub attributes: u64,
    /// Default rendering intent (offset 64).
    pub rendering_intent: RenderingIntent,
    /// PCS illuminant (offset 68); the spec mandates D50 ≈ (0.9642, 1.0, 0.8249).
    pub pcs_illuminant: XyzNumber,
    /// Profile creator signature (offset 80), or [`Signature::ZERO`].
    pub creator: Signature,
    /// Profile ID (offset 84): an MD5 of the profile with certain fields zeroed, or all-zero if
    /// unset (see [`ProfileId::is_zero`]).
    pub profile_id: ProfileId,
    /// The 28 reserved bytes (offset 100). The spec requires zero; preserved verbatim so a
    /// round-trip reproduces even non-conformant inputs exactly.
    pub reserved: [u8; 28],
}

impl ProfileHeader {
    /// A header with spec-valid defaults for a newly built profile: version 4.4.0, an XYZ PCS, the
    /// perceptual rendering intent, the mandated D50 PCS illuminant (§7.2.16), and every
    /// open-registry field unset ([`Signature::ZERO`] / zero).
    ///
    /// The writer computes the `size` field and emits the `acsp` magic itself, so
    /// `ProfileHeader::new` plus [`crate::IccProfile::to_bytes`] yields spec-valid bytes with no
    /// further setup. Special cases — a DeviceLink's device-space PCS, a v2 target, a creation
    /// timestamp — adjust the public fields directly.
    #[must_use]
    pub fn new(device_class: DeviceClass, data_color_space: ColorSpace) -> Self {
        Self {
            size: 0,
            preferred_cmm: Signature::ZERO,
            version: ProfileVersion {
                major: 4,
                minor: 4,
                bugfix: 0,
            },
            device_class,
            data_color_space,
            pcs: ColorSpace::Xyz,
            created: DateTime::ZERO,
            platform: Signature::ZERO,
            flags: 0,
            manufacturer: Signature::ZERO,
            model: Signature::ZERO,
            attributes: 0,
            rendering_intent: RenderingIntent::Perceptual,
            pcs_illuminant: XyzNumber::D50,
            creator: Signature::ZERO,
            profile_id: ProfileId::ZERO,
            reserved: [0; 28],
        }
    }

    /// Parses the 128-byte header from the start of an ICC profile.
    ///
    /// # Errors
    ///
    /// Returns [`IccError::MalformedAt`] if `bytes` is shorter than 128 bytes, the `acsp` signature
    /// is missing, or a closed-registry field (device class, colour space, rendering intent) holds
    /// an unrecognized value. Its offset is the failing field's (§7.2 Table 18), or the input's
    /// length for a short header.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 128 {
            return Err(
                IccError::Malformed("icc: profile shorter than 128-byte header")
                    .located(bytes.len(), None),
            );
        }
        let mut r = ByteReader::new(bytes);
        let size = r.u32()?;
        let preferred_cmm = r.signature()?;
        let major = r.u8()?;
        let minor_bugfix = r.u8()?;
        r.skip(2)?; // the two reserved version bytes
        let version = ProfileVersion::from_bytes(major, minor_bugfix);
        let device_class =
            DeviceClass::try_from(r.signature()?).map_err(|e| e.located(12, None))?;
        let data_color_space =
            ColorSpace::try_from(r.signature()?).map_err(|e| e.located(16, None))?;
        let pcs = ColorSpace::try_from(r.signature()?).map_err(|e| e.located(20, None))?;
        let created = r.date_time()?;
        if r.signature()? != Signature(*b"acsp") {
            return Err(
                IccError::Malformed("icc: missing 'acsp' profile signature").located(36, None)
            );
        }
        let platform = r.signature()?;
        let flags = r.u32()?;
        let manufacturer = r.signature()?;
        let model = r.signature()?;
        let attributes = r.u64()?;
        let rendering_intent =
            RenderingIntent::try_from(r.u32()?).map_err(|e| e.located(64, None))?;
        let pcs_illuminant = r.xyz_number()?;
        let creator = r.signature()?;
        let mut profile_id = [0u8; 16];
        profile_id.copy_from_slice(r.bytes(16)?);
        let mut reserved = [0u8; 28];
        reserved.copy_from_slice(r.bytes(28)?);
        Ok(Self {
            size,
            preferred_cmm,
            version,
            device_class,
            data_color_space,
            pcs,
            created,
            platform,
            flags,
            manufacturer,
            model,
            attributes,
            rendering_intent,
            pcs_illuminant,
            creator,
            profile_id: ProfileId(profile_id),
            reserved,
        })
    }

    /// Whether the profile-flags field marks this profile as embedded in a file
    /// (§7.2.11 Table 21, bit 0).
    #[must_use]
    pub fn is_embedded(&self) -> bool {
        self.flags & (1 << 0) != 0
    }

    /// Whether the profile-flags field marks this profile as unusable independently of the embedded
    /// colour data (§7.2.11 Table 21, bit 1).
    #[must_use]
    pub fn cannot_be_used_independently(&self) -> bool {
        self.flags & (1 << 1) != 0
    }

    /// Whether the device-attributes field marks the media as transparency rather than reflective
    /// (§7.2.14 Table 22, bit 0).
    #[must_use]
    pub fn is_transparency(&self) -> bool {
        self.attributes & (1 << 0) != 0
    }

    /// Whether the device-attributes field marks the media as matte rather than glossy
    /// (§7.2.14 Table 22, bit 1).
    #[must_use]
    pub fn is_matte(&self) -> bool {
        self.attributes & (1 << 1) != 0
    }

    /// Whether the device-attributes field marks the media polarity as negative rather than positive
    /// (§7.2.14 Table 22, bit 2).
    #[must_use]
    pub fn is_negative_polarity(&self) -> bool {
        self.attributes & (1 << 2) != 0
    }

    /// Whether the device-attributes field marks the media as black-and-white rather than colour
    /// (§7.2.14 Table 22, bit 3).
    #[must_use]
    pub fn is_black_and_white(&self) -> bool {
        self.attributes & (1 << 3) != 0
    }

    /// Serializes the 128-byte header. The `size` field is written as stored; the profile writer
    /// patches it to the final length and (optionally) stamps the recomputed profile ID.
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.size.to_be_bytes());
        out.extend_from_slice(&self.preferred_cmm.0);
        out.push(self.version.major);
        out.push(self.version.minor_bugfix_byte());
        out.extend_from_slice(&[0, 0]); // reserved version bytes
        out.extend_from_slice(&Signature::from(self.device_class).0);
        out.extend_from_slice(&Signature::from(self.data_color_space).0);
        out.extend_from_slice(&Signature::from(self.pcs).0);
        push_date_time(out, self.created);
        out.extend_from_slice(b"acsp");
        out.extend_from_slice(&self.platform.0);
        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(&self.manufacturer.0);
        out.extend_from_slice(&self.model.0);
        out.extend_from_slice(&self.attributes.to_be_bytes());
        out.extend_from_slice(&u32::from(self.rendering_intent).to_be_bytes());
        push_xyz_number(out, self.pcs_illuminant);
        out.extend_from_slice(&self.creator.0);
        out.extend_from_slice(&self.profile_id.0);
        out.extend_from_slice(&self.reserved);
    }
}

/// An ICC profile format version, e.g. 4.4.0 or 2.1.0 (ICC.1:2022 §7.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProfileVersion {
    /// Major version (the first version byte; `2` or `4` in practice).
    pub major: u8,
    /// Minor version (the high nibble of the second version byte).
    pub minor: u8,
    /// Bug-fix version (the low nibble of the second version byte).
    pub bugfix: u8,
}

impl ProfileVersion {
    /// Decodes the version from its two significant bytes (the other two are reserved).
    fn from_bytes(major: u8, minor_bugfix: u8) -> Self {
        Self {
            major,
            minor: minor_bugfix >> 4,
            bugfix: minor_bugfix & 0x0F,
        }
    }

    /// Re-encodes the second version byte: minor in the high nibble, bug-fix in the low nibble.
    /// (`+` rather than `|` — the nibbles are disjoint so they are equal, but `+` keeps the
    /// arithmetic mutation-testable.)
    fn minor_bugfix_byte(self) -> u8 {
        ((self.minor & 0x0F) << 4) + (self.bugfix & 0x0F)
    }
}

impl core::fmt::Display for ProfileVersion {
    /// `major.minor.bugfix`, e.g. `4.4.0`.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.bugfix)
    }
}

/// The profile/device class (ICC.1:2022 §7.2.5, Table 18).
///
/// Each discriminant is the class's four-byte signature as a big-endian `u32` — the header word
/// itself — so `class as u32` is the value a C caller writes and reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum DeviceClass {
    /// `scnr` — input device (scanner, camera).
    Input = u32::from_be_bytes(*b"scnr"),
    /// `mntr` — display device (monitor).
    Display = u32::from_be_bytes(*b"mntr"),
    /// `prtr` — output device (printer).
    Output = u32::from_be_bytes(*b"prtr"),
    /// `link` — a device link (a fused device-to-device transform).
    DeviceLink = u32::from_be_bytes(*b"link"),
    /// `spac` — a colour-space conversion profile.
    ColorSpace = u32::from_be_bytes(*b"spac"),
    /// `abst` — an abstract profile.
    Abstract = u32::from_be_bytes(*b"abst"),
    /// `nmcl` — a named-colour profile.
    NamedColor = u32::from_be_bytes(*b"nmcl"),
}

impl DeviceClass {
    /// Every class §7.2.5 defines, in Table 18's order.
    const ALL: [Self; 7] = [
        Self::Input,
        Self::Display,
        Self::Output,
        Self::DeviceLink,
        Self::ColorSpace,
        Self::Abstract,
        Self::NamedColor,
    ];
}

impl TryFrom<Signature> for DeviceClass {
    type Error = IccError;

    /// Maps a class signature to its variant.
    ///
    /// # Errors
    ///
    /// Returns [`IccError::Malformed`] if the signature is not one of the seven defined classes.
    fn try_from(sig: Signature) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|&class| Signature::from(class) == sig)
            .ok_or(IccError::Malformed("icc: unknown device class"))
    }
}

impl From<DeviceClass> for Signature {
    /// The four-byte signature for the class.
    fn from(class: DeviceClass) -> Signature {
        Signature::from(class as u32)
    }
}

/// A colour-space signature (ICC.1:2022 §7.2.6, Table 19), used for both the data colour space
/// and the PCS.
///
/// Each discriminant is the space's four-byte signature as a big-endian `u32` — the header word
/// itself — so `space as u32` is the value a C caller writes and reads. The `nCLR` family is one
/// variant per colorant count, [`NColor2`](Self::NColor2) (`2CLR`) to
/// [`NColor15`](Self::NColor15) (`FCLR`). Marked `#[non_exhaustive]`: ICC revisions have grown
/// Table 19 before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
#[non_exhaustive]
pub enum ColorSpace {
    /// `XYZ ` — CIE XYZ (a valid PCS).
    Xyz = u32::from_be_bytes(*b"XYZ "),
    /// `Lab ` — CIE L\*a\*b\* (a valid PCS).
    Lab = u32::from_be_bytes(*b"Lab "),
    /// `Luv ` — CIE L\*u\*v\*.
    Luv = u32::from_be_bytes(*b"Luv "),
    /// `YCbr` — YCbCr.
    YCbCr = u32::from_be_bytes(*b"YCbr"),
    /// `Yxy ` — CIE Yxy.
    Yxy = u32::from_be_bytes(*b"Yxy "),
    /// `RGB ` — RGB.
    Rgb = u32::from_be_bytes(*b"RGB "),
    /// `GRAY` — grayscale.
    Gray = u32::from_be_bytes(*b"GRAY"),
    /// `HSV ` — HSV.
    Hsv = u32::from_be_bytes(*b"HSV "),
    /// `HLS ` — HLS.
    Hls = u32::from_be_bytes(*b"HLS "),
    /// `CMYK` — CMYK.
    Cmyk = u32::from_be_bytes(*b"CMYK"),
    /// `CMY ` — CMY.
    Cmy = u32::from_be_bytes(*b"CMY "),
    /// `2CLR` — two-colour.
    NColor2 = u32::from_be_bytes(*b"2CLR"),
    /// `3CLR` — three-colour.
    NColor3 = u32::from_be_bytes(*b"3CLR"),
    /// `4CLR` — four-colour.
    NColor4 = u32::from_be_bytes(*b"4CLR"),
    /// `5CLR` — five-colour.
    NColor5 = u32::from_be_bytes(*b"5CLR"),
    /// `6CLR` — six-colour.
    NColor6 = u32::from_be_bytes(*b"6CLR"),
    /// `7CLR` — seven-colour.
    NColor7 = u32::from_be_bytes(*b"7CLR"),
    /// `8CLR` — eight-colour.
    NColor8 = u32::from_be_bytes(*b"8CLR"),
    /// `9CLR` — nine-colour.
    NColor9 = u32::from_be_bytes(*b"9CLR"),
    /// `ACLR` — ten-colour.
    NColor10 = u32::from_be_bytes(*b"ACLR"),
    /// `BCLR` — eleven-colour.
    NColor11 = u32::from_be_bytes(*b"BCLR"),
    /// `CCLR` — twelve-colour.
    NColor12 = u32::from_be_bytes(*b"CCLR"),
    /// `DCLR` — thirteen-colour.
    NColor13 = u32::from_be_bytes(*b"DCLR"),
    /// `ECLR` — fourteen-colour.
    NColor14 = u32::from_be_bytes(*b"ECLR"),
    /// `FCLR` — fifteen-colour.
    NColor15 = u32::from_be_bytes(*b"FCLR"),
}

impl ColorSpace {
    /// Every space §7.2.6 defines, in Table 19's order, closing with the `nCLR` family.
    const ALL: [Self; 25] = [
        Self::Xyz,
        Self::Lab,
        Self::Luv,
        Self::YCbCr,
        Self::Yxy,
        Self::Rgb,
        Self::Gray,
        Self::Hsv,
        Self::Hls,
        Self::Cmyk,
        Self::Cmy,
        Self::NColor2,
        Self::NColor3,
        Self::NColor4,
        Self::NColor5,
        Self::NColor6,
        Self::NColor7,
        Self::NColor8,
        Self::NColor9,
        Self::NColor10,
        Self::NColor11,
        Self::NColor12,
        Self::NColor13,
        Self::NColor14,
        Self::NColor15,
    ];

    /// The number of device channels (components) a colour in this space carries — the sample
    /// count per pixel a CMM transform for this space consumes or produces (ICC.1:2022 Table 19).
    #[must_use]
    pub fn channel_count(&self) -> u8 {
        match self {
            Self::Xyz
            | Self::Lab
            | Self::Luv
            | Self::YCbCr
            | Self::Yxy
            | Self::Rgb
            | Self::Hsv
            | Self::Hls
            | Self::Cmy => 3,
            Self::Gray => 1,
            Self::Cmyk => 4,
            Self::NColor2 => 2,
            Self::NColor3 => 3,
            Self::NColor4 => 4,
            Self::NColor5 => 5,
            Self::NColor6 => 6,
            Self::NColor7 => 7,
            Self::NColor8 => 8,
            Self::NColor9 => 9,
            Self::NColor10 => 10,
            Self::NColor11 => 11,
            Self::NColor12 => 12,
            Self::NColor13 => 13,
            Self::NColor14 => 14,
            Self::NColor15 => 15,
        }
    }

    /// Whether this is one of the `nCLR` multi-colorant spaces ([`NColor2`](Self::NColor2) to
    /// [`NColor15`](Self::NColor15)) — the "xCLR" family §8.5.2 and §8.6 condition their colorant
    /// table requirements on.
    #[must_use]
    pub fn is_n_color(self) -> bool {
        Signature::from(self).0[1..] == *b"CLR"
    }
}

impl TryFrom<Signature> for ColorSpace {
    type Error = IccError;

    /// Maps a colour-space signature to its variant.
    ///
    /// # Errors
    ///
    /// Returns [`IccError::Malformed`] if the signature is neither a defined space nor a valid
    /// `nCLR` (2–15 colorants).
    fn try_from(sig: Signature) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|&space| Signature::from(space) == sig)
            .ok_or(IccError::Malformed("icc: unknown colour space"))
    }
}

impl From<ColorSpace> for Signature {
    /// The four-byte signature for the colour space.
    fn from(space: ColorSpace) -> Signature {
        Signature::from(space as u32)
    }
}

/// The default rendering intent (ICC.1:2022 §7.2.15, Table 23).
///
/// Each discriminant is the intent's header word, so `intent as u32` is the value a C caller
/// writes and reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum RenderingIntent {
    /// `0` — perceptual.
    Perceptual = 0,
    /// `1` — media-relative colorimetric.
    MediaRelativeColorimetric = 1,
    /// `2` — saturation.
    Saturation = 2,
    /// `3` — ICC-absolute colorimetric.
    IccAbsoluteColorimetric = 3,
}

impl TryFrom<u32> for RenderingIntent {
    type Error = IccError;

    /// Maps the header's rendering-intent word to its variant.
    ///
    /// # Errors
    ///
    /// Returns [`IccError::Malformed`] for values outside 0–3 (the upper 16 bits must be zero).
    fn try_from(value: u32) -> Result<Self> {
        Ok(match value {
            0 => Self::Perceptual,
            1 => Self::MediaRelativeColorimetric,
            2 => Self::Saturation,
            3 => Self::IccAbsoluteColorimetric,
            _ => return Err(IccError::Malformed("icc: invalid rendering intent")),
        })
    }
}

impl From<RenderingIntent> for u32 {
    /// The rendering-intent word for the variant.
    fn from(intent: RenderingIntent) -> u32 {
        intent as u32
    }
}

/// The 16-byte profile identifier (ICC.1:2022 §7.2.18): an MD5 of the profile with certain fields
/// zeroed, or all-zero when unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProfileId(pub [u8; 16]);

impl ProfileId {
    /// The all-zero ID, meaning "unset".
    pub const ZERO: ProfileId = ProfileId([0; 16]);

    /// Whether the ID is unset (all zero).
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.0 == [0; 16]
    }

    /// Computes the profile ID (ICC.1:2022 §7.2.18): the MD5 of a fully serialized profile with
    /// the profile-flags (bytes 44–47), rendering-intent (64–67) and profile-ID (84–99) fields
    /// zeroed first, as the spec requires.
    #[must_use]
    pub fn compute(profile_bytes: &[u8]) -> ProfileId {
        let mut buf = profile_bytes.to_vec();
        for range in [44..48usize, 64..68, 84..100] {
            if let Some(field) = buf.get_mut(range) {
                field.fill(0);
            }
        }
        ProfileId(Md5::digest(&buf).into())
    }
}

impl core::fmt::Display for ProfileId {
    /// The 16 ID bytes as 32 lowercase hex digits.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::S15Fixed16;

    /// A valid 128-byte header with a distinct value in every field, so a parse test pins each
    /// field's byte offset.
    fn sample_header() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0x0000_1234_u32.to_be_bytes()); // 0: size
        b.extend_from_slice(b"appl"); // 4: preferred CMM
        b.extend_from_slice(&[0x04, 0x30, 0x00, 0x00]); // 8: version 4.3.0
        b.extend_from_slice(b"mntr"); // 12: device class
        b.extend_from_slice(b"RGB "); // 16: data colour space
        b.extend_from_slice(b"XYZ "); // 20: PCS
        for v in [2026u16, 6, 14, 12, 30, 45] {
            b.extend_from_slice(&v.to_be_bytes()); // 24: created
        }
        b.extend_from_slice(b"acsp"); // 36: magic
        b.extend_from_slice(b"APPL"); // 40: platform
        b.extend_from_slice(&0x0000_0001_u32.to_be_bytes()); // 44: flags
        b.extend_from_slice(b"MFGR"); // 48: manufacturer
        b.extend_from_slice(b"MODL"); // 52: model
        b.extend_from_slice(&0x0000_0001_0000_0002_u64.to_be_bytes()); // 56: attributes
        b.extend_from_slice(&1_u32.to_be_bytes()); // 64: rendering intent
        for raw in [0x0000_F6D6_i32, 0x0001_0000, 0x0000_D32D] {
            b.extend_from_slice(&raw.to_be_bytes()); // 68: PCS illuminant (D50)
        }
        b.extend_from_slice(b"crtr"); // 80: creator
        b.extend_from_slice(&(1u8..=16).collect::<Vec<_>>()); // 84: profile ID
        b.extend_from_slice(&[0u8; 28]); // 100: reserved
        assert_eq!(b.len(), 128);
        b
    }

    #[test]
    fn parses_every_field_at_its_offset() {
        let h = ProfileHeader::parse(&sample_header()).unwrap();
        assert_eq!(h.size, 0x0000_1234);
        assert_eq!(h.preferred_cmm, Signature(*b"appl"));
        assert_eq!(
            h.version,
            ProfileVersion {
                major: 4,
                minor: 3,
                bugfix: 0
            }
        );
        assert_eq!(h.device_class, DeviceClass::Display);
        assert_eq!(h.data_color_space, ColorSpace::Rgb);
        assert_eq!(h.pcs, ColorSpace::Xyz);
        assert_eq!(
            h.created,
            DateTime {
                year: 2026,
                month: 6,
                day: 14,
                hours: 12,
                minutes: 30,
                seconds: 45
            }
        );
        assert_eq!(h.platform, Signature(*b"APPL"));
        assert_eq!(h.flags, 1);
        assert_eq!(h.manufacturer, Signature(*b"MFGR"));
        assert_eq!(h.model, Signature(*b"MODL"));
        assert_eq!(h.attributes, 0x0000_0001_0000_0002);
        assert_eq!(
            h.rendering_intent,
            RenderingIntent::MediaRelativeColorimetric
        );
        assert_eq!(
            h.pcs_illuminant,
            XyzNumber {
                x: S15Fixed16(0x0000_F6D6),
                y: S15Fixed16(0x0001_0000),
                z: S15Fixed16(0x0000_D32D),
            }
        );
        assert_eq!(h.creator, Signature(*b"crtr"));
        assert_eq!(
            h.profile_id,
            ProfileId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
        );
        assert_eq!(h.reserved, [0u8; 28]);
    }

    #[test]
    fn decodes_flag_and_attribute_bits() {
        let mut h = ProfileHeader::parse(&sample_header()).unwrap();

        // Profile-flags bits 0 and 1, isolated so each pins its own bit position.
        h.flags = 0b01;
        assert!(h.is_embedded() && !h.cannot_be_used_independently());
        h.flags = 0b10;
        assert!(!h.is_embedded() && h.cannot_be_used_independently());

        // Device-attributes bits 0–3, isolated one at a time.
        h.attributes = 0b0001;
        assert!(h.is_transparency());
        assert!(!h.is_matte() && !h.is_negative_polarity() && !h.is_black_and_white());
        h.attributes = 0b0010;
        assert!(h.is_matte() && !h.is_transparency());
        h.attributes = 0b0100;
        assert!(h.is_negative_polarity() && !h.is_matte());
        h.attributes = 0b1000;
        assert!(h.is_black_and_white() && !h.is_negative_polarity());
        h.attributes = 0;
        assert!(
            !h.is_transparency()
                && !h.is_matte()
                && !h.is_negative_polarity()
                && !h.is_black_and_white()
        );
    }

    /// Table 18's class signatures, written out from the spec rather than derived from the enum.
    const DEVICE_CLASSES: [(DeviceClass, &[u8; 4]); 7] = [
        (DeviceClass::Input, b"scnr"),
        (DeviceClass::Display, b"mntr"),
        (DeviceClass::Output, b"prtr"),
        (DeviceClass::DeviceLink, b"link"),
        (DeviceClass::ColorSpace, b"spac"),
        (DeviceClass::Abstract, b"abst"),
        (DeviceClass::NamedColor, b"nmcl"),
    ];

    /// Table 19's colour-space signatures and channel counts, written out from the spec.
    const COLOR_SPACES: [(ColorSpace, &[u8; 4], u8); 25] = [
        (ColorSpace::Xyz, b"XYZ ", 3),
        (ColorSpace::Lab, b"Lab ", 3),
        (ColorSpace::Luv, b"Luv ", 3),
        (ColorSpace::YCbCr, b"YCbr", 3),
        (ColorSpace::Yxy, b"Yxy ", 3),
        (ColorSpace::Rgb, b"RGB ", 3),
        (ColorSpace::Gray, b"GRAY", 1),
        (ColorSpace::Hsv, b"HSV ", 3),
        (ColorSpace::Hls, b"HLS ", 3),
        (ColorSpace::Cmyk, b"CMYK", 4),
        (ColorSpace::Cmy, b"CMY ", 3),
        (ColorSpace::NColor2, b"2CLR", 2),
        (ColorSpace::NColor3, b"3CLR", 3),
        (ColorSpace::NColor4, b"4CLR", 4),
        (ColorSpace::NColor5, b"5CLR", 5),
        (ColorSpace::NColor6, b"6CLR", 6),
        (ColorSpace::NColor7, b"7CLR", 7),
        (ColorSpace::NColor8, b"8CLR", 8),
        (ColorSpace::NColor9, b"9CLR", 9),
        (ColorSpace::NColor10, b"ACLR", 10),
        (ColorSpace::NColor11, b"BCLR", 11),
        (ColorSpace::NColor12, b"CCLR", 12),
        (ColorSpace::NColor13, b"DCLR", 13),
        (ColorSpace::NColor14, b"ECLR", 14),
        (ColorSpace::NColor15, b"FCLR", 15),
    ];

    #[test]
    fn device_class_signatures_match_table_18() {
        for (class, sig) in DEVICE_CLASSES {
            assert_eq!(Signature::from(class), Signature(*sig), "{class:?}");
            assert_eq!(class as u32, u32::from_be_bytes(*sig), "{class:?}");
            assert_eq!(DeviceClass::try_from(Signature(*sig)).unwrap(), class);
        }
        let err = DeviceClass::try_from(Signature(*b"scnR")).unwrap_err();
        assert_eq!(err.message(), "icc: unknown device class");
    }

    #[test]
    fn color_space_signatures_match_table_19() {
        for (space, sig, _) in COLOR_SPACES {
            assert_eq!(Signature::from(space), Signature(*sig), "{space:?}");
            assert_eq!(space as u32, u32::from_be_bytes(*sig), "{space:?}");
            assert_eq!(ColorSpace::try_from(Signature(*sig)).unwrap(), space);
        }
        // Signatures that resemble `nCLR` but are not in Table 19 are rejected.
        for bad in [
            b"3xLR", b"3CxR", b"3CLx", b"3zzz", b"1CLR", b"GCLR", b"0CLR",
        ] {
            let err = ColorSpace::try_from(Signature(*bad)).unwrap_err();
            assert_eq!(err.message(), "icc: unknown colour space", "{bad:?}");
        }
    }

    #[test]
    fn is_n_color_is_the_clr_family() {
        for (space, sig, _) in COLOR_SPACES {
            assert_eq!(space.is_n_color(), sig[1..] == *b"CLR", "{space:?}");
        }
    }

    #[test]
    fn channel_count_covers_every_colour_space() {
        for (space, _, channels) in COLOR_SPACES {
            assert_eq!(space.channel_count(), channels, "{space:?}");
        }
    }

    #[test]
    fn profile_id_zero_detection() {
        assert!(ProfileId::ZERO.is_zero());
        let mut id = ProfileId::ZERO;
        id.0[7] = 1;
        assert!(!id.is_zero());
    }

    #[test]
    fn profile_id_excludes_the_zeroed_fields() {
        let mut base = sample_header();
        base.extend_from_slice(&0u32.to_be_bytes()); // an empty tag table completes the profile
        let id = ProfileId::compute(&base);

        // The flags (44), rendering-intent (64) and profile-ID (84–99) regions are zeroed first,
        // so changing a byte in any of them leaves the ID unchanged.
        for offset in [44usize, 64, 90] {
            let mut poked = base.clone();
            poked[offset] = 0xFF;
            assert_eq!(
                ProfileId::compute(&poked),
                id,
                "offset {offset} should be excluded from the ID"
            );
        }
        // A byte outside those regions does change the ID.
        let mut other = base.clone();
        other[40] = 0xFF; // primary platform
        assert_ne!(ProfileId::compute(&other), id);
    }

    #[test]
    fn display_renders_version_and_id() {
        let version = ProfileVersion {
            major: 4,
            minor: 4,
            bugfix: 0,
        };
        assert_eq!(version.to_string(), "4.4.0");
        let mut id = ProfileId::ZERO;
        id.0[0] = 0xAB;
        id.0[15] = 0x01;
        assert_eq!(id.to_string(), "ab000000000000000000000000000001");
    }

    #[test]
    fn version_with_bugfix_round_trips_through_write() {
        let mut header = ProfileHeader::parse(&sample_header()).unwrap();
        header.version = ProfileVersion {
            major: 4,
            minor: 3,
            bugfix: 2,
        };
        let mut out = Vec::new();
        header.write(&mut out);
        let parsed = ProfileHeader::parse(&out).unwrap();
        assert_eq!(
            parsed.version,
            ProfileVersion {
                major: 4,
                minor: 3,
                bugfix: 2,
            }
        );
    }

    #[test]
    fn rendering_intent_round_trip_and_range() {
        for value in 0..=3 {
            let intent = RenderingIntent::try_from(value).unwrap();
            assert_eq!(u32::from(intent), value);
            assert_eq!(intent as u32, value);
        }
        assert!(RenderingIntent::try_from(4u32).is_err());
    }

    #[test]
    fn rejects_malformed_headers_at_the_failing_field() {
        /// The message and offset `parse` reports; a header failure never names a tag.
        fn failure(bytes: &[u8]) -> (&'static str, Option<u64>) {
            let err = ProfileHeader::parse(bytes).unwrap_err();
            assert_eq!(err.tag(), None);
            (err.message(), err.offset())
        }
        // Too short: located where the missing bytes begin.
        assert_eq!(
            failure(&[0u8; 64]),
            ("icc: profile shorter than 128-byte header", Some(64))
        );
        // Bad 'acsp' magic at offset 36.
        let mut bad = sample_header();
        bad[36] = b'X';
        assert_eq!(
            failure(&bad),
            ("icc: missing 'acsp' profile signature", Some(36))
        );
        // Rendering intent out of range at offset 64.
        let mut bad = sample_header();
        bad[67] = 9;
        assert_eq!(failure(&bad), ("icc: invalid rendering intent", Some(64)));
        // Unknown data colour space at offset 16.
        let mut bad = sample_header();
        bad[16..20].copy_from_slice(b"zzzz");
        assert_eq!(failure(&bad), ("icc: unknown colour space", Some(16)));
        // Unknown PCS at offset 20.
        let mut bad = sample_header();
        bad[20..24].copy_from_slice(b"zzzz");
        assert_eq!(failure(&bad), ("icc: unknown colour space", Some(20)));
        // Unknown device class at offset 12.
        let mut bad = sample_header();
        bad[12..16].copy_from_slice(b"zzzz");
        assert_eq!(failure(&bad), ("icc: unknown device class", Some(12)));
    }
}
