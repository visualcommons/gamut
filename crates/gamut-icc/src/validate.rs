//! Profile conformance checking against the required tags of ICC.1:2022 §8 and the header-keyed
//! `MatrixCoefficients` clause of §10.3.
//!
//! [`IccProfile::validate`] reports the requirements a profile does *not* meet. For the §8
//! required-tag sets it is a structural check of tag *presence* only, and it deliberately omits
//! the `chromaticAdaptationTag` requirement (§8.2), which is conditional on measurement data not
//! recoverable from the profile alone. The one tag *content* it evaluates is the §10.3 clause
//! tying a `cicpType` tag's `MatrixCoefficients` to the header's data colour space.

use crate::header::{ColorSpace, DeviceClass};
use crate::primitives::Signature;
use crate::profile::IccProfile;
use crate::tag_types::TagData;
use crate::tags::KnownTag;

/// A single ICC.1:2022 requirement a profile does not satisfy: a §8 required tag that is absent,
/// or the §10.3 `MatrixCoefficients` clause on a `cicpType` tag that is present.
///
/// An empty [`Vec<Conformance>`] from [`IccProfile::validate`] means the profile carries every tag
/// its device class requires and its `cicp` tag, if any, agrees with its data colour space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conformance {
    /// A human-readable description of the unmet requirement (with its §8 or §10.3 subsection).
    /// For §10.3 it names the header's data colour space and the offending `MatrixCoefficients`.
    pub requirement: String,
    /// The tag signatures whose absence caused it. For an alternative-model requirement these are
    /// the tags missing from the nearest-to-complete model. Empty for the §10.3 requirement, which
    /// concerns the contents of a tag that *is* present rather than the absence of one.
    pub missing: Vec<Signature>,
}

/// One alternative profile model for a device class: a display name and the tags it requires.
type Model = (&'static str, &'static [[u8; 4]]);

/// The Input-profile models (§8.3): N-component LUT-based, three-component matrix-based, monochrome.
const INPUT_MODELS: &[Model] = &[
    ("N-component LUT-based", &[*b"A2B0"]),
    (
        "three-component matrix-based",
        &[*b"rXYZ", *b"gXYZ", *b"bXYZ", *b"rTRC", *b"gTRC", *b"bTRC"],
    ),
    ("monochrome", &[*b"kTRC"]),
];

/// The Display-profile models (§8.4).
const DISPLAY_MODELS: &[Model] = &[
    ("N-component LUT-based", &[*b"A2B0", *b"B2A0"]),
    (
        "three-component matrix-based",
        &[*b"rXYZ", *b"gXYZ", *b"bXYZ", *b"rTRC", *b"gTRC", *b"bTRC"],
    ),
    ("monochrome", &[*b"kTRC"]),
];

/// The Output-profile models (§8.5): N-component LUT-based (plus a conditional `colorantTableTag`,
/// handled separately) and monochrome.
const OUTPUT_MODELS: &[Model] = &[
    (
        "N-component LUT-based",
        &[
            *b"A2B0", *b"A2B1", *b"A2B2", *b"B2A0", *b"B2A1", *b"B2A2", *b"gamt",
        ],
    ),
    ("monochrome", &[*b"kTRC"]),
];

impl IccProfile {
    /// Reports the ICC.1:2022 requirements this profile does not meet: the §8 required tags of its
    /// device class, then the §10.3 `MatrixCoefficients` clause on its `cicp` tag. An empty result
    /// means it carries every required tag and any `cicp` tag agrees with the header.
    ///
    /// The §8 checks are of tag *presence* only, and omit the conditional
    /// `chromaticAdaptationTag` requirement. Profiles routinely carry additional optional tags;
    /// those are never reported.
    ///
    /// §10.3 is reported with the same weight as a missing required tag, because ICC states both
    /// of its directions as *shall*: a `cicp` tag in an RGB or XYZ profile whose
    /// `MatrixCoefficients` is non-zero (an `nclx` box copied verbatim produces one), or one in a
    /// YCbCr profile whose `MatrixCoefficients` is zero. Nothing refuses such a profile — it
    /// parses, round-trips and transforms as before; `validate` only names the clause it breaks.
    #[must_use]
    pub fn validate(&self) -> Vec<Conformance> {
        let mut issues = Vec::new();

        // §8.2 — every class except DeviceLink shares these three required tags.
        if self.header.device_class != DeviceClass::DeviceLink {
            self.require_each(
                &mut issues,
                "§8.2 common requirements",
                &[
                    (*b"desc", "profileDescriptionTag"),
                    (*b"cprt", "copyrightTag"),
                    (*b"wtpt", "mediaWhitePointTag"),
                ],
            );
        }

        match self.header.device_class {
            DeviceClass::Input => self.require_model(&mut issues, "§8.3 Input", INPUT_MODELS),
            DeviceClass::Display => self.require_model(&mut issues, "§8.4 Display", DISPLAY_MODELS),
            DeviceClass::Output => {
                self.require_model(&mut issues, "§8.5 Output", OUTPUT_MODELS);
                self.require_output_colorant_table(&mut issues);
            }
            DeviceClass::DeviceLink => self.require_device_link(&mut issues),
            DeviceClass::ColorSpace => self.require_each(
                &mut issues,
                "§8.7 ColorSpace profile",
                &[(*b"A2B0", "AToB0Tag"), (*b"B2A0", "BToA0Tag")],
            ),
            DeviceClass::Abstract => {
                self.require_each(
                    &mut issues,
                    "§8.8 Abstract profile",
                    &[(*b"A2B0", "AToB0Tag")],
                );
            }
            DeviceClass::NamedColor => self.require_each(
                &mut issues,
                "§8.9 NamedColor profile",
                &[(*b"ncl2", "namedColor2Tag")],
            ),
        }

        self.require_cicp_matrix_coefficients(&mut issues);
        issues
    }

    /// §10.3 — "When the data colour space in the profile header is RGB or XYZ, MatrixCoefficients
    /// shall be 0 (zero). When the data colour space in the profile header is YCbCr,
    /// MatrixCoefficients shall be non-zero." Every other data colour space is unconstrained by
    /// this clause (§9.2.17 forbids the tag there outright, a separate requirement not checked
    /// here). A `cicp` tag that did not decode as a `cicpType` has no field to read.
    fn require_cicp_matrix_coefficients(&self, issues: &mut Vec<Conformance>) {
        let Some(TagData::Cicp(cicp)) = self.get(KnownTag::Cicp) else {
            return;
        };
        let matrix = cicp.matrix_coefficients;
        let space = self.header.data_color_space;
        let shall = match space {
            ColorSpace::Rgb | ColorSpace::Xyz if matrix != 0 => "shall be 0 (zero)",
            ColorSpace::YCbCr if matrix == 0 => "shall be non-zero",
            _ => return,
        };
        issues.push(Conformance {
            requirement: format!(
                "§10.3 cicpType with a '{}' data colour space: MatrixCoefficients {shall} (is {matrix})",
                Signature::from(space)
            ),
            missing: Vec::new(),
        });
    }

    /// Whether a tag with the given four-byte signature is present.
    fn has(&self, code: [u8; 4]) -> bool {
        self.get(Signature(code)).is_some()
    }

    /// Records a [`Conformance`] for each named tag in `tags` that is absent.
    fn require_each(&self, issues: &mut Vec<Conformance>, section: &str, tags: &[([u8; 4], &str)]) {
        for &(code, name) in tags {
            if !self.has(code) {
                issues.push(Conformance {
                    requirement: format!("{section}: {name} ({})", Signature(code)),
                    missing: vec![Signature(code)],
                });
            }
        }
    }

    /// Records a [`Conformance`] if none of the class's alternative models is complete, reporting the
    /// tags missing from the nearest-to-complete model.
    fn require_model(&self, issues: &mut Vec<Conformance>, section: &str, models: &[Model]) {
        let mut nearest: Option<(&str, Vec<Signature>)> = None;
        for &(name, codes) in models {
            let missing: Vec<Signature> = codes
                .iter()
                .filter(|&&code| !self.has(code))
                .map(|&code| Signature(code))
                .collect();
            if missing.is_empty() {
                return; // a complete model satisfies the requirement
            }
            if nearest
                .as_ref()
                .is_none_or(|(_, best)| missing.len() < best.len())
            {
                nearest = Some((name, missing));
            }
        }
        if let Some((model_name, missing)) = nearest {
            let options = models
                .iter()
                .map(|&(name, _)| name)
                .collect::<Vec<_>>()
                .join(" | ");
            issues.push(Conformance {
                requirement: format!(
                    "{section} profile requires a complete tag model (one of: {options}); nearest is \"{model_name}\""
                ),
                missing,
            });
        }
    }

    /// §8.5.2 — an N-component LUT-based Output profile with an xCLR data colour space also requires
    /// the `colorantTableTag`.
    fn require_output_colorant_table(&self, issues: &mut Vec<Conformance>) {
        if matches!(self.header.data_color_space, ColorSpace::NColor(_))
            && self.has(*b"A2B0")
            && !self.has(*b"clrt")
        {
            issues.push(Conformance {
                requirement:
                    "§8.5.2 Output profile with an xCLR data colour space: colorantTableTag (clrt)"
                        .to_owned(),
                missing: vec![Signature(*b"clrt")],
            });
        }
    }

    /// §8.6 — DeviceLink profiles have their own required-tag set (no `mediaWhitePointTag`), plus
    /// conditional colorant tables for xCLR data/PCS spaces.
    fn require_device_link(&self, issues: &mut Vec<Conformance>) {
        self.require_each(
            issues,
            "§8.6 DeviceLink profile",
            &[
                (*b"desc", "profileDescriptionTag"),
                (*b"cprt", "copyrightTag"),
                (*b"pseq", "profileSequenceDescTag"),
                (*b"A2B0", "AToB0Tag"),
            ],
        );
        if matches!(self.header.data_color_space, ColorSpace::NColor(_)) && !self.has(*b"clrt") {
            issues.push(Conformance {
                requirement:
                    "§8.6 DeviceLink with an xCLR data colour space: colorantTableTag (clrt)"
                        .to_owned(),
                missing: vec![Signature(*b"clrt")],
            });
        }
        if matches!(self.header.pcs, ColorSpace::NColor(_)) && !self.has(*b"clot") {
            issues.push(Conformance {
                requirement: "§8.6 DeviceLink with an xCLR PCS: colorantTableOutTag (clot)"
                    .to_owned(),
                missing: vec![Signature(*b"clot")],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cicp::Cicp;
    use crate::header::ProfileHeader;

    /// A header for `class`/`data` space with otherwise-neutral fields.
    fn header(class: DeviceClass, data: ColorSpace, pcs: ColorSpace) -> ProfileHeader {
        let mut h = ProfileHeader::new(class, data);
        h.pcs = pcs;
        h
    }

    /// A profile of `class`/`data`→`pcs` carrying (empty) tags for each of `tags`.
    fn profile(
        class: DeviceClass,
        data: ColorSpace,
        pcs: ColorSpace,
        tags: &[&[u8; 4]],
    ) -> IccProfile {
        IccProfile {
            header: header(class, data, pcs),
            tags: tags
                .iter()
                .map(|&&code| {
                    (
                        Signature(code),
                        TagData::Raw {
                            type_sig: Signature(code),
                            bytes: Vec::new(),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn complete_matrix_display_profile_conforms() {
        let p = profile(
            DeviceClass::Display,
            ColorSpace::Rgb,
            ColorSpace::Xyz,
            &[
                b"desc", b"cprt", b"wtpt", b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC", b"bTRC",
            ],
        );
        assert!(p.validate().is_empty());
    }

    #[test]
    fn monochrome_display_profile_conforms() {
        let p = profile(
            DeviceClass::Display,
            ColorSpace::Gray,
            ColorSpace::Xyz,
            &[b"desc", b"cprt", b"wtpt", b"kTRC"],
        );
        assert!(p.validate().is_empty());
    }

    #[test]
    fn bare_display_profile_reports_common_and_model_gaps() {
        let p = profile(DeviceClass::Display, ColorSpace::Rgb, ColorSpace::Xyz, &[]);
        let issues = p.validate();
        // The three §8.2 common tags plus one model requirement.
        assert_eq!(issues.len(), 4);
        // The nearest model to an empty profile is monochrome (a single missing tag).
        let model = issues.last().unwrap();
        assert_eq!(model.missing, vec![Signature(*b"kTRC")]);
    }

    #[test]
    fn device_link_needs_pseq_and_atob0_but_not_white_point() {
        // A DeviceLink with its four required tags conforms even without a mediaWhitePointTag.
        let ok = profile(
            DeviceClass::DeviceLink,
            ColorSpace::Rgb,
            ColorSpace::Cmyk,
            &[b"desc", b"cprt", b"pseq", b"A2B0"],
        );
        assert!(ok.validate().is_empty());

        // Missing profileSequenceDescTag is reported.
        let missing_pseq = profile(
            DeviceClass::DeviceLink,
            ColorSpace::Rgb,
            ColorSpace::Cmyk,
            &[b"desc", b"cprt", b"A2B0"],
        );
        let issues = missing_pseq.validate();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].missing, vec![Signature(*b"pseq")]);
    }

    #[test]
    fn named_color_profile_requires_ncl2() {
        let p = profile(
            DeviceClass::NamedColor,
            ColorSpace::Rgb,
            ColorSpace::Xyz,
            &[b"desc", b"cprt", b"wtpt"],
        );
        let issues = p.validate();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].missing, vec![Signature(*b"ncl2")]);
    }

    #[test]
    fn nearest_model_prefers_the_first_on_a_tie() {
        // One tag missing from each Input model (A2B0 / bTRC / kTRC): a three-way tie, which
        // reports the first-listed model (N-component LUT-based) as nearest.
        let p = profile(
            DeviceClass::Input,
            ColorSpace::Rgb,
            ColorSpace::Xyz,
            &[
                b"desc", b"cprt", b"wtpt", b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC",
            ],
        );
        let issues = p.validate();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].missing, vec![Signature(*b"A2B0")]);
    }

    #[test]
    fn device_link_with_xclr_colorant_tables_conforms() {
        // xCLR data and PCS spaces with both colorant tables present: fully conformant — the
        // clrt/clot requirements fire only on absence.
        let with = profile(
            DeviceClass::DeviceLink,
            ColorSpace::NColor(4),
            ColorSpace::NColor(4),
            &[b"desc", b"cprt", b"pseq", b"A2B0", b"clrt", b"clot"],
        );
        assert!(with.validate().is_empty());

        // Without them, both conditional requirements are reported.
        let without = profile(
            DeviceClass::DeviceLink,
            ColorSpace::NColor(4),
            ColorSpace::NColor(4),
            &[b"desc", b"cprt", b"pseq", b"A2B0"],
        );
        let issues = without.validate();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].missing, vec![Signature(*b"clrt")]);
        assert_eq!(issues[1].missing, vec![Signature(*b"clot")]);
    }

    #[test]
    fn xclr_output_profile_requires_colorant_table() {
        let tags: &[&[u8; 4]] = &[
            b"desc", b"cprt", b"wtpt", b"A2B0", b"A2B1", b"A2B2", b"B2A0", b"B2A1", b"B2A2",
            b"gamt",
        ];
        // A 4-colorant (xCLR) Output profile without clrt is non-conformant...
        let without = profile(
            DeviceClass::Output,
            ColorSpace::NColor(4),
            ColorSpace::Xyz,
            tags,
        );
        let issues = without.validate();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].missing, vec![Signature(*b"clrt")]);

        // ...but conformant once colorantTableTag is present.
        let mut with_tags = tags.to_vec();
        with_tags.push(b"clrt");
        let with = profile(
            DeviceClass::Output,
            ColorSpace::NColor(4),
            ColorSpace::Xyz,
            &with_tags,
        );
        assert!(with.validate().is_empty());
    }

    /// A §8-complete matrix Display profile with a `data` colour space, carrying a decoded `cicp`
    /// tag whose only non-default field is `MatrixCoefficients`.
    fn with_cicp(data: ColorSpace, matrix_coefficients: u8) -> IccProfile {
        let mut p = profile(
            DeviceClass::Display,
            data,
            ColorSpace::Xyz,
            &[
                b"desc", b"cprt", b"wtpt", b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC", b"bTRC",
            ],
        );
        p.tags.push((
            KnownTag::Cicp.into(),
            TagData::Cicp(Cicp {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients,
                video_full_range_flag: 1,
            }),
        ));
        p
    }

    /// The §10.3 finding for `space`, as `validate` words it.
    fn matrix_finding(space: &str, shall: &str, is: u8) -> Conformance {
        Conformance {
            requirement: format!(
                "§10.3 cicpType with a '{space}' data colour space: MatrixCoefficients {shall} (is {is})"
            ),
            missing: vec![],
        }
    }

    #[test]
    fn rgb_cicp_matrix_coefficients_shall_be_zero() {
        assert_eq!(with_cicp(ColorSpace::Rgb, 0).validate(), vec![]);
        assert_eq!(
            with_cicp(ColorSpace::Rgb, 6).validate(),
            vec![matrix_finding("RGB ", "shall be 0 (zero)", 6)]
        );
    }

    #[test]
    fn xyz_cicp_matrix_coefficients_shall_be_zero() {
        assert_eq!(with_cicp(ColorSpace::Xyz, 0).validate(), vec![]);
        assert_eq!(
            with_cicp(ColorSpace::Xyz, 1).validate(),
            vec![matrix_finding("XYZ ", "shall be 0 (zero)", 1)]
        );
    }

    #[test]
    fn ycbcr_cicp_matrix_coefficients_shall_be_non_zero() {
        assert_eq!(with_cicp(ColorSpace::YCbCr, 1).validate(), vec![]);
        assert_eq!(
            with_cicp(ColorSpace::YCbCr, 0).validate(),
            vec![matrix_finding("YCbr", "shall be non-zero", 0)]
        );
    }

    /// §10.3 keys the clause on RGB, XYZ and YCbCr only; under any other data colour space neither
    /// value of `MatrixCoefficients` is a §10.3 finding.
    #[test]
    fn other_space_cicp_matrix_coefficients_are_unconstrained() {
        assert_eq!(with_cicp(ColorSpace::Gray, 0).validate(), vec![]);
        assert_eq!(with_cicp(ColorSpace::Gray, 6).validate(), vec![]);
    }
}
