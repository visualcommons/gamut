//! The output picture size an HEVC sequence parameter set declares — what
//! [`AbiHevcDecoder`](crate::AbiHevcDecoder) sizes the planes it lends a backend by.
//!
//! An `hvcC` record carries no picture size, and the item's `ispe` is not the codestream's to
//! state, so the size is read where the decoder itself reads it: `pic_width_in_luma_samples` and
//! `pic_height_in_luma_samples` of the SPS (ITU-T H.265 §7.3.2.2.1), less the conformance window
//! (§7.4.3.2.1), which is the cropped picture a decoder outputs. Only the SPS prefix up to the
//! conformance window is read.

use gamut_bitstream::BitReader;
use gamut_core::{Dimensions, Error, Result};

use crate::hvcc::HevcConfig;
use crate::nal::{NalHeader, NalUnitType, iter_nal_units};

/// The refusal when neither the `hvcC` record nor the payload carries an SPS.
pub(crate) const NO_SPS: &str = "HEIC: no SPS states the coded picture size";

/// The refusal when the conformance window crops the whole picture away.
const EMPTY_CROP: &str = "HEIC: the SPS conformance window crops the whole picture";

/// The bits of the general part of `profile_tier_level()` (§7.3.3): profile space, tier, profile
/// idc, the 32 compatibility flags, the four source flags, the 43 constraint bits, the
/// `general_inbld_flag`/reserved bit and `general_level_idc`.
const GENERAL_PROFILE_TIER_LEVEL_BITS: usize = 2 + 1 + 5 + 32 + 4 + 43 + 1 + 8;

/// The bits of one sub-layer's profile part of `profile_tier_level()`, the same fields as the
/// general part's less `general_level_idc`.
const SUB_LAYER_PROFILE_BITS: usize = GENERAL_PROFILE_TIER_LEVEL_BITS - 8;

/// The bits of one sub-layer's `sub_layer_level_idc`.
const SUB_LAYER_LEVEL_BITS: usize = 8;

/// The cropped output size of the picture coded under `config`, from the first SPS of the record
/// or, failing that (an `hev1` item may carry its parameter sets inband), of `payload`.
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when no SPS is
/// present, when the SPS is truncated, or when it states an empty picture.
pub(crate) fn output_size(config: &HevcConfig, payload: &[u8]) -> Result<Dimensions> {
    if let Some(sps) = config.sps().next() {
        return parse(sps);
    }
    for nal in iter_nal_units(payload, config.nal_length_size()) {
        let nal = nal?;
        if NalHeader::parse(nal)?.unit_type == NalUnitType::Sps {
            return parse(nal);
        }
    }
    Err(invalid(NO_SPS))
}

/// The cropped output size one SPS NAL unit (header included) declares.
fn parse(nal: &[u8]) -> Result<Dimensions> {
    let rbsp = rbsp(nal.get(2..).unwrap_or_default());
    let mut r = BitReader::new(&rbsp);
    // sps_video_parameter_set_id u(4); sps_max_sub_layers_minus1 u(3);
    // sps_temporal_id_nesting_flag u(1).
    r.skip_bits(4)?;
    let sub_layers = r.get_bits(3)? as usize;
    r.skip_bits(1)?;
    skip_profile_tier_level(&mut r, sub_layers)?;
    // sps_seq_parameter_set_id ue(v).
    r.uvlc()?;
    let chroma_format_idc = r.uvlc()?;
    if chroma_format_idc == 3 {
        // separate_colour_plane_flag u(1). Either way 4:4:4 crops in luma units (SubWidthC and
        // SubHeightC are 1, Table 6-1), so its value is not needed.
        r.skip_bits(1)?;
    }
    let (sub_width, sub_height) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = u64::from(r.uvlc()?);
    let height = u64::from(r.uvlc()?);
    let (mut crop_x, mut crop_y) = (0, 0);
    if r.flag()? {
        // conf_win_left_offset, conf_win_right_offset, conf_win_top_offset,
        // conf_win_bottom_offset, each ue(v), in chroma units (§7.4.3.2.1).
        crop_x = sub_width * (u64::from(r.uvlc()?) + u64::from(r.uvlc()?));
        crop_y = sub_height * (u64::from(r.uvlc()?) + u64::from(r.uvlc()?));
    }
    let cropped = |full: u64, crop: u64| {
        full.checked_sub(crop)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| invalid(EMPTY_CROP))
    };
    Dimensions::new(cropped(width, crop_x)?, cropped(height, crop_y)?)
        .map_err(|_| invalid(EMPTY_CROP))
}

/// Skips `profile_tier_level(1, sub_layers)` (§7.3.3).
fn skip_profile_tier_level(r: &mut BitReader<'_>, sub_layers: usize) -> Result<()> {
    r.skip_bits(GENERAL_PROFILE_TIER_LEVEL_BITS)?;
    let mut present = [(false, false); 7];
    for flags in present.iter_mut().take(sub_layers) {
        *flags = (r.flag()?, r.flag()?);
    }
    if sub_layers > 0 {
        // reserved_zero_2bits for each of the 8 - sub_layers absent sub-layers.
        r.skip_bits(2 * (8 - sub_layers))?;
    }
    for &(profile, level) in present.iter().take(sub_layers) {
        if profile {
            r.skip_bits(SUB_LAYER_PROFILE_BITS)?;
        }
        if level {
            r.skip_bits(SUB_LAYER_LEVEL_BITS)?;
        }
    }
    Ok(())
}

/// The RBSP of a NAL unit body: every `emulation_prevention_three_byte` — a `0x03` after two
/// zero bytes (§7.3.1.1, §7.4.2) — removed.
fn rbsp(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut zeros = 0;
    for &byte in body {
        if zeros == 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { (zeros + 1).min(2) } else { 0 };
        out.push(byte);
    }
    out
}

fn invalid(message: &'static str) -> Error {
    Error::invalid_input(env!("CARGO_PKG_NAME"), message)
}

#[cfg(test)]
mod tests {
    use gamut_bitstream::BitWriter;

    use super::*;

    /// The SPS fields this walk reads, and a sub-layer count with its present flags.
    struct Sps {
        sub_layers: Vec<(bool, bool)>,
        chroma_format_idc: u32,
        separate_planes: bool,
        size: (u32, u32),
        window: Option<[u32; 4]>,
    }

    fn ue(w: &mut BitWriter, value: u32) {
        let coded = u64::from(value) + 1;
        let bits = 64 - coded.leading_zeros();
        w.put_bits(0, bits - 1);
        w.put_bits(coded as u32, bits);
    }

    /// The `profile_tier_level()` field widths (§7.3.3) the fixture writes, spelled out
    /// independently of the walk's constants so a defect in those cannot shift writer and reader
    /// together: 2 + 1 + 5 + 32 + 4 + 43 + 1 bits of profile, then 8 of `general_level_idc`.
    const GENERAL_BITS: usize = 96;
    const SUB_LAYER_PROFILE_BITS_SPEC: usize = 88;

    impl Sps {
        fn new(width: u32, height: u32) -> Self {
            Self {
                sub_layers: Vec::new(),
                chroma_format_idc: 1,
                separate_planes: false,
                size: (width, height),
                window: None,
            }
        }

        /// The NAL unit, header included, with emulation prevention applied.
        fn nal(&self) -> Vec<u8> {
            let mut w = BitWriter::new();
            w.put_bits(0, 4);
            w.put_bits(self.sub_layers.len() as u32, 3);
            w.put_bit(1);
            // The general profile, filled with ones so a misplaced skip reads them as a field.
            for _ in 0..GENERAL_BITS {
                w.put_bit(1);
            }
            for &(profile, level) in &self.sub_layers {
                w.put_bit(u8::from(profile));
                w.put_bit(u8::from(level));
            }
            if !self.sub_layers.is_empty() {
                w.put_bits(0, 2 * (8 - self.sub_layers.len() as u32));
            }
            for &(profile, level) in &self.sub_layers {
                if profile {
                    for _ in 0..SUB_LAYER_PROFILE_BITS_SPEC {
                        w.put_bit(1);
                    }
                }
                if level {
                    w.put_bits(0xFF, 8);
                }
            }
            ue(&mut w, 0);
            ue(&mut w, self.chroma_format_idc);
            if self.chroma_format_idc == 3 {
                w.put_bit(u8::from(self.separate_planes));
            }
            ue(&mut w, self.size.0);
            ue(&mut w, self.size.1);
            w.put_bit(u8::from(self.window.is_some()));
            for offset in self.window.iter().flatten() {
                ue(&mut w, *offset);
            }
            // rbsp_trailing_bits is not read; pad to a byte.
            w.put_bit(1);
            w.align();
            let mut nal = vec![0x42, 0x01];
            let mut zeros = 0;
            for byte in w.into_bytes() {
                if zeros == 2 && byte <= 3 {
                    nal.push(3);
                    zeros = 0;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                nal.push(byte);
            }
            nal
        }
    }

    fn size(sps: &Sps) -> Result<(u32, u32)> {
        parse(&sps.nal()).map(|d| (d.width(), d.height()))
    }

    #[test]
    fn the_uncropped_size_is_the_luma_picture_size() {
        assert_eq!(size(&Sps::new(64, 48)).unwrap(), (64, 48));
    }

    #[test]
    fn the_conformance_window_crops_in_chroma_units() {
        let window = Some([1, 2, 3, 4]);
        for (idc, separate, (x, y)) in [
            (0, false, (1, 1)),
            (1, false, (2, 2)),
            (2, false, (2, 1)),
            (3, false, (1, 1)),
            (3, true, (1, 1)),
        ] {
            let sps = Sps {
                chroma_format_idc: idc,
                separate_planes: separate,
                window,
                ..Sps::new(64, 48)
            };
            assert_eq!(size(&sps).unwrap(), (64 - 3 * x, 48 - 7 * y), "idc {idc}");
        }
    }

    #[test]
    fn sub_layer_profile_and_level_fields_are_skipped() {
        let sps = Sps {
            sub_layers: vec![(true, false), (false, true), (true, true)],
            ..Sps::new(40, 24)
        };
        assert_eq!(size(&sps).unwrap(), (40, 24));
        let one = Sps {
            sub_layers: vec![(false, false)],
            ..Sps::new(40, 24)
        };
        assert_eq!(size(&one).unwrap(), (40, 24));
    }

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), [0, 0, 1, 0, 0, 0, 0]);
        // A 0x03 after a single zero, or after a run broken by a non-zero byte, is data.
        assert_eq!(rbsp(&[0, 3, 0, 1, 0, 3]), [0, 3, 0, 1, 0, 3]);
        // After a removed byte the zero run starts again.
        assert_eq!(rbsp(&[0, 0, 3, 3]), [0, 0, 3]);
        // A run longer than two zeros still ends in one removal.
        assert_eq!(rbsp(&[0, 0, 0, 3, 7]), [0, 0, 0, 7]);
    }

    #[test]
    fn a_window_cropping_everything_or_a_truncated_sps_is_refused() {
        let empty = Sps {
            chroma_format_idc: 0,
            window: Some([32, 32, 0, 0]),
            ..Sps::new(64, 48)
        };
        assert_eq!(
            parse(&empty.nal()).unwrap_err().static_message(),
            Some(EMPTY_CROP)
        );
        let past = Sps {
            chroma_format_idc: 0,
            window: Some([40, 40, 0, 0]),
            ..Sps::new(64, 48)
        };
        assert_eq!(
            parse(&past.nal()).unwrap_err().static_message(),
            Some(EMPTY_CROP)
        );
        let nal = Sps::new(64, 48).nal();
        assert!(parse(&nal[..8]).is_err());
    }

    fn config(arrays: &[(u8, &[u8])]) -> HevcConfig {
        let mut raw = vec![0u8; 23];
        raw[0] = 1;
        raw[21] = 0b11;
        raw[22] = arrays.len() as u8;
        for &(ty, nal) in arrays {
            raw.push(ty);
            raw.extend_from_slice(&1u16.to_be_bytes());
            raw.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            raw.extend_from_slice(nal);
        }
        HevcConfig::parse(&raw).unwrap()
    }

    fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| {
                (nal.len() as u32)
                    .to_be_bytes()
                    .into_iter()
                    .chain(nal.iter().copied())
            })
            .collect()
    }

    #[test]
    fn the_records_sps_wins_and_an_inband_one_is_the_fallback() {
        let record = Sps::new(16, 8).nal();
        let inband = Sps::new(32, 24).nal();
        let payload = length_prefixed(&[&[0x26, 0x01, 0xAA], &inband]);
        let dims =
            |config: &HevcConfig| output_size(config, &payload).map(|d| (d.width(), d.height()));
        assert_eq!(dims(&config(&[(33, &record)])).unwrap(), (16, 8));
        // A VPS in the record is not an SPS; the payload's is used.
        assert_eq!(
            dims(&config(&[(32, &[0x40, 0x01, 0x0C])])).unwrap(),
            (32, 24)
        );
        let err = output_size(&config(&[]), &length_prefixed(&[&[0x26, 0x01, 0xAA]])).unwrap_err();
        assert_eq!(err.static_message(), Some(NO_SPS));
    }
}
