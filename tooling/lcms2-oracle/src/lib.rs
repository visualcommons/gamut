//! Dev-only differential oracle around a vendored, statically-linked **Little-CMS (lcms2)**.
//!
//! `gamut-icc` must parse the ICC profiles a reference CMM writes and re-serialize profiles that
//! CMM accepts as equivalent, and `gamut-color` must reproduce that CMM's colorimetry. This crate
//! wraps lcms2 (built from the `third_party/lcms2` submodule) behind a small, safe API:
//!
//! * **synthesis** ([`synth`]) — build a diverse corpus of valid profiles *in memory*, so no
//!   binary `.icc` fixtures need committing: [`srgb`], [`rgb_matrix_shaper`], [`gray`], [`xyz`],
//!   [`lab4`], [`lab2`], the LUT-bearing [`scnr_lut`] / [`cmyk_prtr_v4`] / [`cmyk_prtr_v2`], …;
//! * **inspection** — open a profile blob and read back header fields and decoded tag values
//!   ([`Profile::from_bytes`], [`Profile::color_space`], [`Profile::read_xyz`], …);
//! * **transforms** ([`xform`]) — [`Transform`] over `cmsCreateTransform` and friends, pixel
//!   format codes ([`TYPE_RGB_DBL`], …), transform flags ([`FLAGS_NOCACHE`], …), and black-point
//!   detection;
//! * **colorimetry** ([`color`], [`curves`]) — ΔE metrics, XYZ↔Lab, the fixed-point PCS
//!   encoders, and standalone [`ToneCurve`]s.
//!
//! Profiles work entirely in RAM via `cmsSaveProfileToMem`/`cmsOpenProfileFromMem`, so — unlike the
//! file-based libtiff/DNG oracles — there is no temp-file round-trip. All `unsafe` FFI is confined
//! to this crate; returned values are copied out of lcms2's memory before the handle is closed.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

use std::os::raw::c_char;
use std::ptr;

mod sys {
    // Generated bindings: vendored, machine-emitted code we do not lint.
    #![allow(warnings, clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub mod color;
pub mod curves;
pub mod synth;
pub mod xform;

pub use color::{
    cie2000_delta_e, delta_e_76, lab_decode_v2, lab_decode_v4, lab_encode_v2, lab_encode_v4,
    lab_to_xyz, xyz_decode, xyz_encode, xyz_to_lab,
};
pub use curves::ToneCurve;
pub use synth::{
    cicp, clut_probe_profile, cmyk_ink_limiting_devicelink, cmyk_prtr_v2, cmyk_prtr_v4,
    display_p3_srgb_trc, gray, lab2, lab4, measurement, rgb_linearization_devicelink,
    rgb_matrix_shaper, rgb_matrix_shaper_d65_wtpt, rgb_matrix_shaper_v2, scnr_lut,
    scnr_matrix_shaper, srgb, viewing_conditions, xyz,
};
pub use xform::{
    ClutPipeline, FLAGS_BLACKPOINTCOMPENSATION, FLAGS_GAMUTCHECK, FLAGS_HIGHRESPRECALC,
    FLAGS_LOWRESPRECALC, FLAGS_NOCACHE, FLAGS_NOOPTIMIZE, FLAGS_NOWHITEONWHITEFIXUP,
    FLAGS_SOFTPROOFING, INTENT_ABSOLUTE_COLORIMETRIC, INTENT_PERCEPTUAL,
    INTENT_RELATIVE_COLORIMETRIC, INTENT_SATURATION, TYPE_CMYK_8, TYPE_CMYK_16, TYPE_CMYK_DBL,
    TYPE_CMYK_FLT, TYPE_GRAY_8, TYPE_GRAY_16, TYPE_GRAY_DBL, TYPE_Lab_16, TYPE_Lab_DBL,
    TYPE_LabV2_16, TYPE_RGB_8, TYPE_RGB_16, TYPE_RGB_DBL, TYPE_RGB_FLT, TYPE_XYZ_16, TYPE_XYZ_DBL,
    Transform, detect_black_point, detect_destination_black_point, set_alarm_codes,
    set_quiet_log_handler,
};

/// ICC tag signatures (a four-character code as a big-endian `u32`) accepted by the `tag` argument
/// of the read-back methods. Mirrors the subset of `cmsTagSignature` the cross-checks exercise.
pub mod tag {
    const fn sig(b: &[u8; 4]) -> u32 {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }
    /// `rXYZ` — red colorant (`XYZType`).
    pub const RED_COLORANT: u32 = sig(b"rXYZ");
    /// `gXYZ` — green colorant.
    pub const GREEN_COLORANT: u32 = sig(b"gXYZ");
    /// `bXYZ` — blue colorant.
    pub const BLUE_COLORANT: u32 = sig(b"bXYZ");
    /// `wtpt` — media white point.
    pub const MEDIA_WHITE_POINT: u32 = sig(b"wtpt");
    /// `rTRC` — red tone-response curve.
    pub const RED_TRC: u32 = sig(b"rTRC");
    /// `gTRC` — green tone-response curve.
    pub const GREEN_TRC: u32 = sig(b"gTRC");
    /// `bTRC` — blue tone-response curve.
    pub const BLUE_TRC: u32 = sig(b"bTRC");
    /// `kTRC` — grey tone-response curve.
    pub const GRAY_TRC: u32 = sig(b"kTRC");
    /// `desc` — profile description.
    pub const PROFILE_DESCRIPTION: u32 = sig(b"desc");
    /// `cprt` — copyright.
    pub const COPYRIGHT: u32 = sig(b"cprt");
    /// `chad` — chromatic-adaptation matrix.
    pub const CHROMATIC_ADAPTATION: u32 = sig(b"chad");
    /// `A2B0` — the device-to-PCS lookup transform (perceptual).
    pub const A_TO_B0: u32 = sig(b"A2B0");
    /// `A2B1` — the device-to-PCS lookup transform (relative colorimetric).
    pub const A_TO_B1: u32 = sig(b"A2B1");
    /// `A2B2` — the device-to-PCS lookup transform (saturation).
    pub const A_TO_B2: u32 = sig(b"A2B2");
    /// `B2A0` — the PCS-to-device lookup transform (perceptual).
    pub const B_TO_A0: u32 = sig(b"B2A0");
    /// `B2A1` — the PCS-to-device lookup transform (relative colorimetric).
    pub const B_TO_A1: u32 = sig(b"B2A1");
    /// `B2A2` — the PCS-to-device lookup transform (saturation).
    pub const B_TO_A2: u32 = sig(b"B2A2");
}

/// A four-character colour-space / class signature as a big-endian `u32`, for comparing against the
/// values returned by [`Profile::color_space`], [`Profile::pcs`], and [`Profile::device_class`].
#[must_use]
pub const fn fourcc(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// An owned lcms2 profile handle (`cmsHPROFILE`); closed on drop.
pub struct Profile {
    pub(crate) raw: sys::cmsHPROFILE,
}

impl Drop for Profile {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: `raw` is a live handle from an lcms2 constructor, closed exactly once here.
            unsafe { sys::cmsCloseProfile(self.raw) };
        }
    }
}

pub(crate) fn wrap(raw: sys::cmsHPROFILE) -> Profile {
    assert!(!raw.is_null(), "lcms2 returned a null profile handle");
    Profile { raw }
}

impl Profile {
    /// Force the encoded profile version (e.g. `2.1`, `4.3`), so synthesis can emit legacy v2
    /// layouts (`textDescriptionType`, v2 LUTs) for the cross-checks.
    pub fn set_version(&self, version: f64) {
        // SAFETY: `raw` is a live handle.
        unsafe { sys::cmsSetProfileVersion(self.raw, version) };
    }

    /// Serialize this profile to ICC bytes (`cmsSaveProfileToMem`, two-call size-then-fill).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut needed: sys::cmsUInt32Number = 0;
        // SAFETY: a null buffer with a valid out-param requests the size.
        let ok = unsafe { sys::cmsSaveProfileToMem(self.raw, ptr::null_mut(), &mut needed) };
        assert!(
            ok != 0 && needed > 0,
            "cmsSaveProfileToMem size query failed"
        );
        let mut buf = vec![0u8; needed as usize];
        // SAFETY: `buf` has room for `needed` bytes; lcms writes exactly that many.
        let ok =
            unsafe { sys::cmsSaveProfileToMem(self.raw, buf.as_mut_ptr().cast(), &mut needed) };
        assert!(ok != 0, "cmsSaveProfileToMem write failed");
        buf.truncate(needed as usize);
        buf
    }

    /// Open an ICC byte blob with lcms2 (`cmsOpenProfileFromMem`). Returns `None` if lcms2 rejects
    /// the bytes — the basis of the round-trip gate ("does the reference CMM accept our output?").
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Profile> {
        let len = sys::cmsUInt32Number::try_from(bytes.len()).ok()?;
        // SAFETY: `bytes` is valid for `len` bytes; lcms copies what it needs.
        let raw = unsafe { sys::cmsOpenProfileFromMem(bytes.as_ptr().cast(), len) };
        (!raw.is_null()).then_some(Profile { raw })
    }

    /// The profile version as a float (e.g. `4.3`), via `cmsGetProfileVersion`.
    #[must_use]
    pub fn version(&self) -> f64 {
        // SAFETY: `raw` is a live handle.
        unsafe { sys::cmsGetProfileVersion(self.raw) }
    }

    /// The device-class signature as a big-endian `u32` (e.g. `mntr`), via `cmsGetDeviceClass`.
    #[must_use]
    pub fn device_class(&self) -> u32 {
        // SAFETY: `raw` is a live handle.
        unsafe { sys::cmsGetDeviceClass(self.raw) as u32 }
    }

    /// The data colour-space signature as a big-endian `u32` (e.g. `RGB `), via `cmsGetColorSpace`.
    #[must_use]
    pub fn color_space(&self) -> u32 {
        // SAFETY: `raw` is a live handle.
        unsafe { sys::cmsGetColorSpace(self.raw) as u32 }
    }

    /// The profile-connection-space signature as a big-endian `u32` (`XYZ ` or `Lab `), via
    /// `cmsGetPCS`.
    #[must_use]
    pub fn pcs(&self) -> u32 {
        // SAFETY: `raw` is a live handle.
        unsafe { sys::cmsGetPCS(self.raw) as u32 }
    }

    /// The header's default rendering intent (0–3), via `cmsGetHeaderRenderingIntent`.
    #[must_use]
    pub fn rendering_intent(&self) -> u32 {
        // SAFETY: `raw` is a live handle.
        unsafe { sys::cmsGetHeaderRenderingIntent(self.raw) }
    }

    /// The header flags word, via `cmsGetHeaderFlags`.
    #[must_use]
    pub fn header_flags(&self) -> u32 {
        // SAFETY: `raw` is a live handle.
        unsafe { sys::cmsGetHeaderFlags(self.raw) }
    }

    /// The 16-byte profile ID currently stored in the header, via `cmsGetHeaderProfileID`.
    #[must_use]
    pub fn profile_id(&self) -> [u8; 16] {
        let mut id = [0u8; 16];
        // SAFETY: `raw` is live; lcms writes exactly 16 bytes into `id`.
        unsafe { sys::cmsGetHeaderProfileID(self.raw, id.as_mut_ptr()) };
        id
    }

    /// Recompute the profile ID (MD5) per the spec and return it (`cmsMD5computeID` then read back).
    #[must_use]
    pub fn compute_md5_id(&self) -> [u8; 16] {
        // SAFETY: `raw` is a live handle.
        unsafe { sys::cmsMD5computeID(self.raw) };
        self.profile_id()
    }

    /// The number of tags the profile carries (`cmsGetTagCount`).
    #[must_use]
    pub fn tag_count(&self) -> usize {
        // SAFETY: `raw` is a live handle; a negative count would signal an error.
        let n = unsafe { sys::cmsGetTagCount(self.raw) };
        n.max(0) as usize
    }

    /// The signature of the `n`-th tag as a big-endian `u32` (`cmsGetTagSignature`).
    #[must_use]
    pub fn tag_signature(&self, n: usize) -> u32 {
        // SAFETY: `raw` is live; `n` is bounded by `tag_count` at the call sites.
        unsafe { sys::cmsGetTagSignature(self.raw, n as u32) as u32 }
    }

    /// Whether the profile carries tag `tag` (`cmsIsTag`).
    #[must_use]
    pub fn has_tag(&self, tag: u32) -> bool {
        // SAFETY: `raw` is a live handle.
        unsafe { sys::cmsIsTag(self.raw, tag as sys::cmsTagSignature) != 0 }
    }

    /// Read an `XYZType` tag as `[X, Y, Z]`, via `cmsReadTag`. `None` if the tag is absent.
    #[must_use]
    pub fn read_xyz(&self, tag: u32) -> Option<[f64; 3]> {
        // SAFETY: `raw` is live; the returned pointer is borrowed and copied out immediately.
        let p = unsafe { sys::cmsReadTag(self.raw, tag as sys::cmsTagSignature) }
            as *const sys::cmsCIEXYZ;
        if p.is_null() {
            return None;
        }
        // SAFETY: non-null pointer to a live `cmsCIEXYZ` owned by the profile.
        let v = unsafe { *p };
        Some([v.X, v.Y, v.Z])
    }

    /// Evaluate a tone-curve tag at `x ∈ [0, 1]` (`cmsEvalToneCurveFloat`). `None` if absent.
    #[must_use]
    pub fn eval_tone_curve(&self, tag: u32, x: f32) -> Option<f32> {
        // SAFETY: `raw` is live; the curve pointer is borrowed for the call only.
        let c = unsafe { sys::cmsReadTag(self.raw, tag as sys::cmsTagSignature) }
            as *const sys::cmsToneCurve;
        if c.is_null() {
            return None;
        }
        // SAFETY: non-null borrowed curve owned by the profile.
        Some(unsafe { sys::cmsEvalToneCurveFloat(c, x) })
    }

    /// Estimate the gamma of a tone-curve tag (`cmsEstimateGamma`). `None` if absent or non-power.
    #[must_use]
    pub fn estimate_gamma(&self, tag: u32, precision: f64) -> Option<f64> {
        // SAFETY: `raw` is live; the curve pointer is borrowed for the call only.
        let c = unsafe { sys::cmsReadTag(self.raw, tag as sys::cmsTagSignature) }
            as *const sys::cmsToneCurve;
        if c.is_null() {
            return None;
        }
        // SAFETY: non-null borrowed curve owned by the profile.
        let g = unsafe { sys::cmsEstimateGamma(c, precision) };
        (g > 0.0).then_some(g)
    }

    /// Read a `multiLocalizedUnicodeType`/`textDescriptionType` tag as an ASCII string for the
    /// given language/country (e.g. `b"en"`, `b"US"`), via `cmsMLUgetASCII`. `None` if absent.
    #[must_use]
    pub fn read_mlu_ascii(&self, tag: u32, lang: &[u8; 2], country: &[u8; 2]) -> Option<String> {
        // lcms language/country codes are 2 letters in a 3-byte (NUL-padded) field.
        let lang = [lang[0] as c_char, lang[1] as c_char, 0];
        let country = [country[0] as c_char, country[1] as c_char, 0];
        // SAFETY: `raw` is live; the MLU pointer is borrowed and only read during this call.
        let m =
            unsafe { sys::cmsReadTag(self.raw, tag as sys::cmsTagSignature) } as *const sys::cmsMLU;
        if m.is_null() {
            return None;
        }
        // SAFETY: size query — a null buffer returns the byte count needed.
        let need =
            unsafe { sys::cmsMLUgetASCII(m, lang.as_ptr(), country.as_ptr(), ptr::null_mut(), 0) };
        if need == 0 {
            return None;
        }
        let mut buf = vec![0u8; need as usize];
        // SAFETY: `buf` holds `need` bytes; lcms writes a NUL-terminated ASCII string into it.
        unsafe {
            sys::cmsMLUgetASCII(
                m,
                lang.as_ptr(),
                country.as_ptr(),
                buf.as_mut_ptr().cast(),
                need,
            );
        }
        if buf.last() == Some(&0) {
            buf.pop();
        }
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_round_trips_through_lcms() {
        let p = srgb();
        let bytes = p.to_bytes();
        assert!(bytes.len() > 128, "serialized profile too small");
        // 'acsp' magic at offset 36.
        assert_eq!(&bytes[36..40], b"acsp");
        let reopened = Profile::from_bytes(&bytes).expect("lcms2 re-opens its own output");
        assert_eq!(reopened.color_space(), fourcc(b"RGB "));
        assert_eq!(reopened.pcs(), fourcc(b"XYZ "));
        // The white point colorant is present and close to D50.
        let wtpt = reopened
            .read_xyz(tag::MEDIA_WHITE_POINT)
            .expect("wtpt present");
        assert!(
            (wtpt[1] - 1.0).abs() < 1e-3,
            "wtpt Y ≈ 1.0, got {}",
            wtpt[1]
        );
    }

    #[test]
    fn rejects_non_icc_bytes() {
        assert!(Profile::from_bytes(b"not an icc profile").is_none());
    }
}
