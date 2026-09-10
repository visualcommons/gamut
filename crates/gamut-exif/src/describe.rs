//! Human-readable descriptions of the enumerated tag values, in CIPA DC-008's own wording.
//!
//! Available under the **`describe`** Cargo feature, which is off by default: a display table is
//! several kilobytes of strings a consumer that only writes metadata never reads.
//!
//! An EXIF tag such as `Orientation` or `MeteringMode` stores a small integer code whose meaning
//! the specification fixes. [`describe`] maps `(tag, code)` to the text CIPA DC-008 gives for it,
//! and [`described_values`] gives the whole domain the spec defines for a tag — which is also what
//! makes a code *reserved*: a code outside that set has no defined meaning and [`describe`] returns
//! `None`.
//!
//! ```
//! use gamut_exif::{ExifTag, describe::{describe, described_values}};
//!
//! assert_eq!(describe(ExifTag::ResolutionUnit, 2), Some("inches"));
//! assert_eq!(describe(ExifTag::ResolutionUnit, 4), None); // reserved
//! assert_eq!(described_values(ExifTag::ExposureMode).len(), 3);
//! ```
//!
//! # Reading a code out of a value
//!
//! The code is the tag's numeric value, with two shapes worth naming:
//!
//! * The GPS tags whose codes are **characters** — `GPSStatus`, `GPSMeasureMode` and the nine
//!   ASCII reference tags (`GPSLatitudeRef`, `GPSLongitudeRef`, `GPSSpeedRef`, `GPSTrackRef`,
//!   `GPSImgDirectionRef`, `GPSDestLatitudeRef`, `GPSDestLongitudeRef`, `GPSDestBearingRef`,
//!   `GPSDestDistanceRef`) — are keyed by the character's byte, so
//!   `describe(ExifTag::GpsStatus, u32::from(b'A'))`. Each is stored as a two-byte ASCII value,
//!   the letter and its terminating NUL; the code is the letter.
//! * `ComponentsConfiguration` holds **four** codes, one per channel; describe each on its own.
//!
//! `Flash` is a bitfield rather than an enumeration, so it is not in these tables: use [`flash`].
//! Four more tags for which the specification prints a table are absent for the same kind of
//! reason — what it enumerates for them is not a single scalar code: `GPSVersionID` and
//! `FlashpixVersion` are fixed multi-byte versions, `YCbCrSubSampling` is a pair whose meaning
//! belongs to its two elements jointly, and `InteroperabilityIndex`'s codes are multi-character
//! ASCII strings.

use crate::tag::ExifTag;

/// The values CIPA DC-008 defines for `tag`, in ascending code order, each with the spec's own
/// description.
///
/// Empty for a tag the specification does not enumerate — including `Flash`, whose bit fields
/// [`flash`] decomposes instead.
#[must_use]
pub fn described_values(tag: ExifTag) -> &'static [(u32, &'static str)] {
    match tag {
        // ---- 0th IFD, CIPA DC-008 §4.6.5 ----
        ExifTag::Compression => &[
            (1, "uncompressed"),
            (6, "JPEG compression (thumbnails only)"),
        ],
        ExifTag::PhotometricInterpretation => &[(2, "RGB"), (6, "YCbCr")],
        ExifTag::Orientation => &[
            (
                1,
                "The 0th row is at the visual top of the image, and the 0th column is the visual \
                 left-hand side.",
            ),
            (
                2,
                "The 0th row is at the visual top of the image, and the 0th column is the visual \
                 right-hand side.",
            ),
            (
                3,
                "The 0th row is at the visual bottom of the image, and the 0th column is the \
                 visual right-hand side.",
            ),
            (
                4,
                "The 0th row is at the visual bottom of the image, and the 0th column is the \
                 visual left-hand side.",
            ),
            (
                5,
                "The 0th row is the visual left-hand side of the image, and the 0th column is the \
                 visual top.",
            ),
            (
                6,
                "The 0th row is the visual right-hand side of the image, and the 0th column is the \
                 visual top.",
            ),
            (
                7,
                "The 0th row is the visual right-hand side of the image, and the 0th column is the \
                 visual bottom.",
            ),
            (
                8,
                "The 0th row is the visual left-hand side of the image, and the 0th column is the \
                 visual bottom.",
            ),
        ],
        ExifTag::PlanarConfiguration => &[(1, "chunky format"), (2, "planar format")],
        // §4.6.6.7.28 defines FocalPlaneResolutionUnit as "the same as the ResolutionUnit".
        ExifTag::ResolutionUnit | ExifTag::FocalPlaneResolutionUnit => {
            &[(2, "inches"), (3, "centimeters")]
        }
        ExifTag::YCbCrPositioning => &[(1, "centered"), (2, "co-sited")],

        // ---- Exif sub-IFD, CIPA DC-008 §4.6.6 ----
        ExifTag::ColorSpace => &[(1, "sRGB"), (0xFFFF, "Uncalibrated")],
        ExifTag::ComponentsConfiguration => &[
            (0, "does not exist"),
            (1, "Y"),
            (2, "Cb"),
            (3, "Cr"),
            (4, "R"),
            (5, "G"),
            (6, "B"),
        ],
        ExifTag::ExposureProgram => &[
            (0, "Not defined"),
            (1, "Manual"),
            (2, "Normal program"),
            (3, "Aperture priority"),
            (4, "Shutter priority"),
            (5, "Creative program (biased toward depth of field)"),
            (6, "Action program (biased toward fast shutter speed)"),
            (
                7,
                "Portrait mode (for closeup photos with the background out of focus)",
            ),
            (
                8,
                "Landscape mode (for landscape photos with the background in focus)",
            ),
        ],
        ExifTag::SensitivityType => &[
            (0, "Unknown"),
            (1, "Standard output sensitivity (SOS)"),
            (2, "Recommended exposure index (REI)"),
            (3, "ISO speed"),
            (
                4,
                "Standard output sensitivity (SOS) and recommended exposure index (REI)",
            ),
            (5, "Standard output sensitivity (SOS) and ISO speed"),
            (6, "Recommended exposure index (REI) and ISO speed"),
            (
                7,
                "Standard output sensitivity (SOS) and recommended exposure index (REI) and ISO \
                 speed",
            ),
        ],
        ExifTag::MeteringMode => &[
            (0, "unknown"),
            (1, "Average"),
            (2, "CenterWeightedAverage"),
            (3, "Spot"),
            (4, "Multi-spot"),
            (5, "Pattern"),
            (6, "Partial"),
            (255, "other"),
        ],
        ExifTag::LightSource => &[
            (0, "unknown"),
            (1, "Daylight"),
            (2, "Fluorescent"),
            (3, "Tungsten (incandescent light)"),
            (4, "Flash"),
            (9, "Fine weather"),
            (10, "Cloudy weather"),
            (11, "Shade"),
            (12, "Daylight fluorescent (D 5700 - 7100K)"),
            (13, "Day white fluorescent (N 4600 - 5500K)"),
            (14, "Cool white fluorescent (W 3800 - 4500K)"),
            (15, "White fluorescent (WW 3250 - 3800K)"),
            (16, "Warm white fluorescent (L 2600 - 3250K)"),
            (17, "Standard light A"),
            (18, "Standard light B"),
            (19, "Standard light C"),
            (20, "D55"),
            (21, "D65"),
            (22, "D75"),
            (23, "D50"),
            (24, "ISO studio tungsten"),
            (255, "other light source"),
        ],
        ExifTag::SensingMethod => &[
            (1, "Not defined"),
            (2, "One-chip color area sensor"),
            (3, "Two-chip color area sensor"),
            (4, "Three-chip color area sensor"),
            (5, "Color sequential area sensor"),
            (7, "Trilinear sensor"),
            (8, "Color sequential linear sensor"),
        ],
        ExifTag::FileSource => &[
            (0, "others"),
            (1, "scanner of transparent type"),
            (2, "scanner of reflex type"),
            (3, "DSC"),
        ],
        ExifTag::SceneType => &[(1, "A directly photographed image")],
        ExifTag::CustomRendered => &[(0, "Normal process"), (1, "Custom process")],
        ExifTag::ExposureMode => &[
            (0, "Auto exposure"),
            (1, "Manual exposure"),
            (2, "Auto bracket"),
        ],
        ExifTag::WhiteBalance => &[(0, "Auto white balance"), (1, "Manual white balance")],
        ExifTag::SceneCaptureType => &[
            (0, "Standard"),
            (1, "Landscape"),
            (2, "Portrait"),
            (3, "Night scene"),
        ],
        ExifTag::GainControl => &[
            (0, "None"),
            (1, "Low gain up"),
            (2, "High gain up"),
            (3, "Low gain down"),
            (4, "High gain down"),
        ],
        ExifTag::Contrast | ExifTag::Sharpness => &[(0, "Normal"), (1, "Soft"), (2, "Hard")],
        ExifTag::Saturation => &[(0, "Normal"), (1, "Low saturation"), (2, "High saturation")],
        ExifTag::SubjectDistanceRange => &[
            (0, "unknown"),
            (1, "Macro"),
            (2, "Close view"),
            (3, "Distant view"),
        ],
        ExifTag::CompositeImage => &[
            (0, "unknown"),
            (1, "non-composite image"),
            (2, "General composite image"),
            (3, "Composite image captured when shooting"),
        ],

        // ---- GPS sub-IFD, CIPA DC-008 §4.6.7 ----
        ExifTag::GpsAltitudeRef => &[
            (
                0,
                "Positive ellipsoidal height (at or above ellipsoidal surface)",
            ),
            (1, "Negative ellipsoidal height (below ellipsoidal surface)"),
            (
                2,
                "Positive sea level value (at or above sea level reference)",
            ),
            (3, "Negative sea level value (below sea level reference)"),
        ],
        // The rest of the GPS enumerations are ASCII reference tags, whose codes are *characters*:
        // the key is the character's byte, so 'A' is 65 and 'V' is 86. Each table is in ascending
        // byte order, which is not always the order the spec prints the rows in.
        ExifTag::GpsStatus => &[
            (b'A' as u32, "Measurement in progress"),
            (b'V' as u32, "Measurement interrupted"),
        ],
        ExifTag::GpsMeasureMode => &[
            (b'2' as u32, "2-dimensional measurement"),
            (b'3' as u32, "3-dimensional measurement"),
        ],
        // §4.6.7.1.2 and §4.6.7.1.20 give the shooting location and the destination point the
        // same two codes.
        ExifTag::GpsLatitudeRef | ExifTag::GpsDestLatitudeRef => &[
            (b'N' as u32, "North latitude"),
            (b'S' as u32, "South latitude"),
        ],
        // §4.6.7.1.4 and §4.6.7.1.22, likewise.
        ExifTag::GpsLongitudeRef | ExifTag::GpsDestLongitudeRef => &[
            (b'E' as u32, "East longitude"),
            (b'W' as u32, "West longitude"),
        ],
        // §4.6.7.1.15, §4.6.7.1.17 and §4.6.7.1.24: three directions, one pair of codes.
        ExifTag::GpsTrackRef | ExifTag::GpsImgDirectionRef | ExifTag::GpsDestBearingRef => &[
            (b'M' as u32, "Magnetic direction"),
            (b'T' as u32, "True direction"),
        ],
        // §4.6.7.1.13. Same three letters as `GPSDestDistanceRef`, different units: this one is a
        // speed, so 'N' is knots rather than nautical miles.
        ExifTag::GpsSpeedRef => &[
            (b'K' as u32, "Kilometers per hour"),
            (b'M' as u32, "Miles per hour"),
            (b'N' as u32, "Knots"),
        ],
        // §4.6.7.1.26, the distance counterpart.
        ExifTag::GpsDestDistanceRef => &[
            (b'K' as u32, "Kilometers"),
            (b'M' as u32, "Miles"),
            (b'N' as u32, "Nautical miles"),
        ],
        ExifTag::GpsDifferential => &[
            (0, "Measurement without differential correction"),
            (1, "Differential correction applied"),
        ],

        _ => &[],
    }
}

/// The description CIPA DC-008 gives for `code` in `tag`, or `None` if the spec reserves that code
/// — or does not enumerate the tag at all.
///
/// See the [module docs](self) for how to read a code out of a value.
#[must_use]
pub fn describe(tag: ExifTag, code: u32) -> Option<&'static str> {
    described_values(tag)
        .iter()
        .find(|&&(c, _)| c == code)
        .map(|&(_, text)| text)
}

/// The `Flash` tag's five bit fields, decoded (CIPA DC-008 §4.6.6.7.21, Figure 17).
///
/// `Flash` is a bitfield, not an enumeration, so [`describe`] does not cover it: a single string
/// per value would be a table of every bit combination. Read the fields instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashDescription {
    /// Bit 0 — whether the flash fired.
    pub fired: bool,
    /// Bits 1–2 — the status of returned strobe light.
    pub return_light: &'static str,
    /// Bits 3–4 — the camera's flash mode.
    pub mode: &'static str,
    /// Bit 5 — whether a flash function is present. Note the on-disk sense is inverted: the spec
    /// records `1` for *no* flash function.
    pub function_present: bool,
    /// Bit 6 — whether red-eye reduction is supported.
    pub red_eye_reduction: bool,
}

/// Decodes the `Flash` tag's bit fields.
///
/// Bits above bit 6 are not assigned by CIPA DC-008 and are ignored.
///
/// ```
/// use gamut_exif::describe::flash;
///
/// let f = flash(0x09); // fired, compulsory firing, no return detection
/// assert!(f.fired);
/// assert_eq!(f.mode, "Compulsory flash firing");
/// assert_eq!(f.return_light, "No strobe return detection function");
/// ```
#[must_use]
pub fn flash(bits: u16) -> FlashDescription {
    FlashDescription {
        fired: bits & 0b1 != 0,
        return_light: match (bits >> 1) & 0b11 {
            0 => "No strobe return detection function",
            2 => "Strobe return light not detected.",
            3 => "Strobe return light detected.",
            // 01b is the one reserved combination in Figure 17.
            _ => "reserved",
        },
        mode: match (bits >> 3) & 0b11 {
            0 => "unknown",
            1 => "Compulsory flash firing",
            2 => "Compulsory flash suppression",
            _ => "Auto mode",
        },
        // Figure 17: 0b is "Flash function present", 1b is "No flash function".
        function_present: bits & 0b10_0000 == 0,
        red_eye_reduction: bits & 0b100_0000 != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_reads_the_table_for_the_asked_tag() {
        // The two tags whose codes are easiest to confuse for each other's.
        assert_eq!(describe(ExifTag::ResolutionUnit, 2), Some("inches"));
        assert_eq!(describe(ExifTag::ResolutionUnit, 3), Some("centimeters"));
        assert_eq!(describe(ExifTag::YCbCrPositioning, 2), Some("co-sited"));
        assert_eq!(
            describe(ExifTag::MeteringMode, 2),
            Some("CenterWeightedAverage")
        );
        // A code the spec reserves has no description.
        assert_eq!(describe(ExifTag::ResolutionUnit, 1), None);
        assert_eq!(describe(ExifTag::ResolutionUnit, 4), None);
        // A tag the spec does not enumerate has none either.
        assert_eq!(describe(ExifTag::Make, 1), None);
    }

    #[test]
    fn a_tag_with_a_shared_table_still_gets_its_own_answer() {
        // ResolutionUnit and FocalPlaneResolutionUnit share one arm (§4.6.6.7.28), Contrast and
        // Sharpness another; a wrong arm would silently answer for the sibling tag.
        assert_eq!(
            describe(ExifTag::FocalPlaneResolutionUnit, 3),
            Some("centimeters")
        );
        assert_eq!(describe(ExifTag::Contrast, 1), Some("Soft"));
        assert_eq!(describe(ExifTag::Sharpness, 2), Some("Hard"));
        // Saturation is deliberately *not* in that arm: its 1 and 2 read differently.
        assert_eq!(describe(ExifTag::Saturation, 1), Some("Low saturation"));
        assert_eq!(describe(ExifTag::Saturation, 2), Some("High saturation"));
    }

    #[test]
    fn character_coded_gps_tags_are_keyed_by_the_character() {
        assert_eq!(
            describe(ExifTag::GpsStatus, u32::from(b'A')),
            Some("Measurement in progress")
        );
        assert_eq!(
            describe(ExifTag::GpsMeasureMode, u32::from(b'3')),
            Some("3-dimensional measurement")
        );
        // Not by the digit's numeric value, which is a different code.
        assert_eq!(describe(ExifTag::GpsMeasureMode, 3), None);
    }

    /// The nine ASCII reference tags reuse the same few letters for unrelated meanings, so an arm
    /// that swallowed one tag into a sibling's group would answer plausibly and wrongly. Nothing
    /// else here would notice: the domains are the same size, ascending, and non-empty either way.
    #[test]
    fn a_reference_tag_letter_means_what_its_own_section_says() {
        // 'M' three ways (§4.6.7.1.15, §4.6.7.1.13, §4.6.7.1.26).
        assert_eq!(
            describe(ExifTag::GpsTrackRef, u32::from(b'M')),
            Some("Magnetic direction")
        );
        assert_eq!(
            describe(ExifTag::GpsSpeedRef, u32::from(b'M')),
            Some("Miles per hour")
        );
        assert_eq!(
            describe(ExifTag::GpsDestDistanceRef, u32::from(b'M')),
            Some("Miles")
        );
        // 'N' three ways (§4.6.7.1.2, §4.6.7.1.13, §4.6.7.1.26).
        assert_eq!(
            describe(ExifTag::GpsLatitudeRef, u32::from(b'N')),
            Some("North latitude")
        );
        assert_eq!(
            describe(ExifTag::GpsSpeedRef, u32::from(b'N')),
            Some("Knots")
        );
        assert_eq!(
            describe(ExifTag::GpsDestDistanceRef, u32::from(b'N')),
            Some("Nautical miles")
        );
        // The destination tags repeat the shooting-location codes (§4.6.7.1.20, §4.6.7.1.22),
        // while the bearing reference takes the direction pair (§4.6.7.1.24).
        assert_eq!(
            describe(ExifTag::GpsDestLatitudeRef, u32::from(b'S')),
            Some("South latitude")
        );
        assert_eq!(
            describe(ExifTag::GpsDestLongitudeRef, u32::from(b'W')),
            Some("West longitude")
        );
        assert_eq!(
            describe(ExifTag::GpsLongitudeRef, u32::from(b'E')),
            Some("East longitude")
        );
        assert_eq!(
            describe(ExifTag::GpsDestBearingRef, u32::from(b'T')),
            Some("True direction")
        );
        assert_eq!(
            describe(ExifTag::GpsImgDirectionRef, u32::from(b'T')),
            Some("True direction")
        );
        // A letter one group defines and another does not stays reserved.
        assert_eq!(describe(ExifTag::GpsLatitudeRef, u32::from(b'K')), None);
        assert_eq!(describe(ExifTag::GpsSpeedRef, u32::from(b'T')), None);
    }

    #[test]
    fn the_sixteen_bit_colour_space_code_survives() {
        // Uncalibrated is 0xFFFF, which a narrower key type would have truncated.
        assert_eq!(describe(ExifTag::ColorSpace, 0xFFFF), Some("Uncalibrated"));
        assert_eq!(describe(ExifTag::ColorSpace, 1), Some("sRGB"));
    }

    #[test]
    fn every_table_is_ascending_and_has_no_repeated_code() {
        // A repeated code would make one description unreachable; a mis-sorted table would break
        // any consumer rendering the domain as a menu.
        for &tag in ExifTag::ALL {
            let values = described_values(tag);
            assert!(
                values.windows(2).all(|w| w[0].0 < w[1].0),
                "{} lists codes out of ascending order",
                tag.name()
            );
        }
    }

    #[test]
    fn describe_agrees_with_the_domain_it_publishes() {
        // `describe` must be exactly a lookup into `described_values` - never a second table.
        for &tag in ExifTag::ALL {
            for &(code, text) in described_values(tag) {
                assert_eq!(describe(tag, code), Some(text), "{} = {code}", tag.name());
            }
        }
    }

    /// Drift guard over the rule this module implements: a tag has a table here exactly when
    /// CIPA DC-008 fixes the meaning of a **single scalar code** — one integer, or one ASCII
    /// character — that the value carries on its own, or, for `ComponentsConfiguration`, that
    /// each of its four elements carries on its own.
    ///
    /// So the spec printing a table for a tag is not sufficient. Four tags whose sections print
    /// one are excluded, because what those tables enumerate is not a scalar code: `Flash`
    /// §4.6.6.7.21, a bitfield whose meaning composes independent bits (`flash` decomposes it);
    /// `GPSVersionID` §4.6.7.1.1 and `FlashpixVersion` §4.6.6.1.2, each a fixed multi-byte version
    /// rather than a domain; `YCbCrSubSampling` §4.6.5.1.12, a pair whose meaning belongs to its
    /// two elements jointly; and `InteroperabilityIndex` §4.6.8.1.1, whose codes are
    /// multi-character ASCII strings. Nor is it necessary: `FocalPlaneResolutionUnit` §4.6.6.7.28
    /// is the deliberate exception, admitted with no table of its own because that section defines
    /// it as "the same as the ResolutionUnit" (§4.6.5.1.11) instead of restating those values, and
    /// so shares that arm.
    ///
    /// The list below is what that rule selects — no arm missing, and none invented for a tag the
    /// spec leaves open. Every other assertion in this module reads a table through
    /// `described_values`, so an arm that lost its rows would make them vacuous; this is what
    /// notices.
    #[test]
    fn exactly_the_enumerated_tags_have_a_table() {
        let enumerated: Vec<&str> = ExifTag::ALL
            .iter()
            .filter(|t| !described_values(**t).is_empty())
            .map(|t| t.name())
            .collect();
        assert_eq!(
            enumerated,
            [
                "Compression",
                "PhotometricInterpretation",
                "Orientation",
                "PlanarConfiguration",
                "ResolutionUnit",
                "YCbCrPositioning",
                "ExposureProgram",
                "SensitivityType",
                "ComponentsConfiguration",
                "MeteringMode",
                "LightSource",
                "ColorSpace",
                "FocalPlaneResolutionUnit",
                "SensingMethod",
                "FileSource",
                "SceneType",
                "CustomRendered",
                "ExposureMode",
                "WhiteBalance",
                "SceneCaptureType",
                "GainControl",
                "Contrast",
                "Saturation",
                "Sharpness",
                "SubjectDistanceRange",
                "CompositeImage",
                "GPSLatitudeRef",
                "GPSLongitudeRef",
                "GPSAltitudeRef",
                "GPSStatus",
                "GPSMeasureMode",
                "GPSSpeedRef",
                "GPSTrackRef",
                "GPSImgDirectionRef",
                "GPSDestLatitudeRef",
                "GPSDestLongitudeRef",
                "GPSDestBearingRef",
                "GPSDestDistanceRef",
                "GPSDifferential",
            ]
        );
    }

    #[test]
    fn no_description_is_empty() {
        for &tag in ExifTag::ALL {
            for &(code, text) in described_values(tag) {
                assert!(!text.is_empty(), "{} = {code} has no text", tag.name());
            }
        }
    }

    #[test]
    fn flash_is_not_in_the_enumeration_tables() {
        // It is a bitfield; `flash` decodes it. An entry here would be a partial, misleading one.
        assert_eq!(described_values(ExifTag::Flash), &[]);
        assert_eq!(describe(ExifTag::Flash, 1), None);
    }

    /// Each `Flash` field must depend on its own bits and no others — the law Figure 17 states.
    /// A wrong mask or shift changes some field's answer when an unrelated bit flips.
    #[test]
    fn each_flash_field_depends_only_on_its_own_bits() {
        /// A field's name, the bits Figure 17 assigns to it, and how to read it back as text.
        type Field = (&'static str, u16, fn(FlashDescription) -> String);

        let fields: [Field; 5] = [
            ("fired", 0b000_0001, |f| f.fired.to_string()),
            ("return_light", 0b000_0110, |f| f.return_light.to_owned()),
            ("mode", 0b001_1000, |f| f.mode.to_owned()),
            ("function_present", 0b010_0000, |f| {
                f.function_present.to_string()
            }),
            ("red_eye_reduction", 0b100_0000, |f| {
                f.red_eye_reduction.to_string()
            }),
        ];
        for (name, own, read) in fields {
            for bits in 0..0x80u16 {
                for other in (0..7).map(|b| 1u16 << b).filter(|b| b & own == 0) {
                    assert_eq!(
                        read(flash(bits)),
                        read(flash(bits ^ other)),
                        "{name} changed when bit {other:#b} flipped (from {bits:#09b})"
                    );
                }
            }
        }
    }

    #[test]
    fn flash_reads_each_field_the_way_figure_17_defines_it() {
        // All bits clear: did not fire, no return detection, unknown mode, function present.
        let none = flash(0x00);
        assert!(!none.fired);
        assert_eq!(none.return_light, "No strobe return detection function");
        assert_eq!(none.mode, "unknown");
        assert!(none.function_present, "bit 5 clear means present");
        assert!(!none.red_eye_reduction);

        // Bit 5 set is the inverted one: "No flash function".
        assert!(!flash(0b010_0000).function_present);
        assert!(flash(0b100_0000).red_eye_reduction);
        assert!(flash(0b000_0001).fired);

        // Bits 1-2: 01b is the reserved combination.
        assert_eq!(flash(0b000_0010).return_light, "reserved");
        assert_eq!(
            flash(0b000_0100).return_light,
            "Strobe return light not detected."
        );
        assert_eq!(
            flash(0b000_0110).return_light,
            "Strobe return light detected."
        );

        // Bits 3-4, all four modes.
        assert_eq!(flash(0b000_1000).mode, "Compulsory flash firing");
        assert_eq!(flash(0b001_0000).mode, "Compulsory flash suppression");
        assert_eq!(flash(0b001_1000).mode, "Auto mode");

        // Bits above 6 are unassigned and must not disturb the decode.
        assert_eq!(flash(0xFF80), flash(0x0000));
    }
}
