//! The typed `av1C` AV1CodecConfigurationRecord ([`Av1Config`]) and the config-driven bridges to a
//! downstream AV1 decoder: [`Av1Config::full_stream`] (item payload → self-contained temporal
//! unit), [`Av1Config::validate_still_payload`] (the AVIF still-image constraints), and
//! [`Av1Config::sequence_header`] (the governing AV1 sequence header, parsed by `gamut-av1`).
//!
//! Layout is AV1-ISOBMFF v1.3.0 §2.3.3 exactly (vendored at `references/av1/av1-isobmff`); the
//! payload constraints are AV1-ISOBMFF §2.4 (the sync-sample rules an AV1 image item inherits) and
//! AVIF v1.2.0 §2.1 (`references/avif`). All `reserved` fields are **ignored** on read; they are
//! masked away and never validated.

use gamut_av1::SequenceHeader;
use gamut_color::ChromaSubsampling;
use gamut_core::{Error, Result};

use crate::obu::{ObuType, iter_obus, write_leb128};

/// The chroma sampling format, derived from the `av1C` `monochrome` and
/// `chroma_subsampling_x`/`_y` fields (AV1 §6.4.2: mono / (1,1) = 4:2:0 / (1,0) = 4:2:2 /
/// (0,0) = 4:4:4).
///
/// `#[repr(u8)]` with explicit, stable discriminants so the value is FFI-stable across the boundary
/// a platform decoder crosses (mirroring `gamut-heic`'s C-compatibility posture): a `-sys` shim can
/// pass the raw discriminant without a translation table.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromaFormat {
    /// Monochrome: a single luma plane, no chroma.
    Monochrome = 0,
    /// 4:2:0: chroma sub-sampled by two horizontally and vertically.
    Yuv420 = 1,
    /// 4:2:2: chroma sub-sampled by two horizontally.
    Yuv422 = 2,
    /// 4:4:4: full-resolution chroma.
    Yuv444 = 3,
}

impl ChromaFormat {
    /// The equivalent [`ChromaSubsampling`], gamut's shared plane-geometry vocabulary.
    ///
    /// A total mapping: this type's discriminants are the codec's `chroma_format_idc`, while
    /// `ChromaSubsampling`'s are gamut's own, so the two never share a numbering and convert
    /// explicitly.
    #[must_use]
    pub fn subsampling(self) -> ChromaSubsampling {
        match self {
            ChromaFormat::Monochrome => ChromaSubsampling::Cs400,
            ChromaFormat::Yuv420 => ChromaSubsampling::Cs420,
            ChromaFormat::Yuv422 => ChromaSubsampling::Cs422,
            ChromaFormat::Yuv444 => ChromaSubsampling::Cs444,
        }
    }

    /// The dimensions of each chroma (Cb/Cr) plane for a luma plane of `width` × `height`, using
    /// **ceiling** division on the subsampled axes so an odd luma dimension keeps the half-covering
    /// edge sample: 4:2:0 ⇒ `(ceil(width/2), ceil(height/2))`, 4:2:2 ⇒ `(ceil(width/2), height)`,
    /// 4:4:4 ⇒ `(width, height)`. [`Monochrome`](Self::Monochrome) has no chroma, so it returns
    /// `(0, 0)`.
    #[must_use]
    pub fn chroma_dimensions(self, width: u32, height: u32) -> (u32, u32) {
        self.subsampling().chroma_dimensions(width, height)
    }
}

/// A parsed `av1C` AV1CodecConfigurationRecord (AV1-ISOBMFF v1.3.0 §2.3.3).
///
/// The fixed four header bytes are exposed as typed values (bit-width fields as the smallest
/// integer that holds them), followed by the opaque [`config_obus`](Self::config_obus) stream.
/// Every field mirrors the AV1 sequence header the item payload carries (§2.3.4's cross-box
/// consistency rules); this record is the *container's* copy — the write-direction mirror is
/// [`gamut_av1::Av1StillConfig`], the encoder-side sequence-header parameters the encoder stamps
/// into this record.
///
/// Construct via [`parse`](Self::parse). Non-exhaustive so later fields can be surfaced additively.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Av1Config {
    /// `seq_profile` (3 bits) — 0 = Main (4:2:0/mono), 1 = High (4:4:4), 2 = Professional.
    pub seq_profile: u8,
    /// `seq_level_idx_0` (5 bits) — the level of the first operating point (31 = maximum
    /// parameters).
    pub seq_level_idx_0: u8,
    /// `seq_tier_0` (1 bit) — the tier of the first operating point.
    pub seq_tier_0: u8,
    /// `high_bitdepth` (1 bit) — together with [`twelve_bit`](Self::twelve_bit) and
    /// [`seq_profile`](Self::seq_profile) determines the [`bit_depth`](Self::bit_depth).
    pub high_bitdepth: bool,
    /// `twelve_bit` (1 bit) — only meaningful for profile 2 with `high_bitdepth`.
    pub twelve_bit: bool,
    /// `monochrome` (1 bit) — a single luma plane; requires subsampling `(1, 1)`.
    pub monochrome: bool,
    /// `chroma_subsampling_x` (1 bit).
    pub chroma_subsampling_x: u8,
    /// `chroma_subsampling_y` (1 bit).
    pub chroma_subsampling_y: u8,
    /// `chroma_sample_position` (2 bits) — 0 = unknown, 1 = vertical (left), 2 = co-located
    /// (top-left); meaningful only for 4:2:0.
    pub chroma_sample_position: u8,
    /// `initial_presentation_delay_minus_one` (4 bits), when the record's presence flag is set;
    /// `None` when absent (the alternative 4 reserved bits are ignored).
    pub initial_presentation_delay_minus_one: Option<u8>,
    /// The `configOBUs` bytes: zero or more low-overhead OBUs (at most one sequence header plus
    /// metadata) applying to every sample, kept verbatim. Each has `obu_has_size_field = 1`
    /// (validated at parse, §2.3.4); a sequence header here must match the one in the item payload
    /// — that cross-check is the decoder's concern, not this record's.
    pub config_obus: Vec<u8>,
}

impl Av1Config {
    /// Parses an `av1C` AV1CodecConfigurationRecord from its raw body bytes (the `av1C` item
    /// property, AV1-ISOBMFF v1.3.0 §2.3.3).
    ///
    /// Reserved bits are masked away and never validated. The four fixed header bytes are followed
    /// by `configOBUs[]` to the end of the record, so there is no trailing-byte ambiguity; the
    /// `configOBUs` are validated to split into OBUs that each carry a size field (a §2.3.4 SHALL)
    /// but are otherwise kept verbatim.
    ///
    /// # Errors
    ///
    /// - [`Error::Unsupported`] if `version != 1`.
    /// - [`Error::InvalidInput`] if the record is shorter than four bytes, if `marker != 1` (a
    ///   §2.3.4 SHALL — the marker distinguishes the record from an OBU header byte), if the
    ///   subsampling pair is `(0, 1)` or `monochrome` lacks `(1, 1)` (combinations the AV1
    ///   sequence header cannot express, §6.4.2), or if `configOBUs` does not split into
    ///   size-field-carrying OBUs.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let &[b0, b1, b2, b3, ref config_obus @ ..] = data else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: av1C truncated",
            ));
        };
        // marker(1) | version(7)
        if b0 & 0x80 == 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: av1C marker must be 1",
            ));
        }
        if b0 & 0x7f != 1 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: av1C version must be 1",
            ));
        }
        // seq_profile(3) | seq_level_idx_0(5)
        let seq_profile = b1 >> 5;
        let seq_level_idx_0 = b1 & 0x1f;
        // seq_tier_0(1) | high_bitdepth(1) | twelve_bit(1) | monochrome(1) |
        // chroma_subsampling_x(1) | chroma_subsampling_y(1) | chroma_sample_position(2)
        let seq_tier_0 = b2 >> 7;
        let high_bitdepth = b2 & 0x40 != 0;
        let twelve_bit = b2 & 0x20 != 0;
        let monochrome = b2 & 0x10 != 0;
        let chroma_subsampling_x = (b2 >> 3) & 0x01;
        let chroma_subsampling_y = (b2 >> 2) & 0x01;
        let chroma_sample_position = b2 & 0x03;
        if chroma_subsampling_x == 0 && chroma_subsampling_y == 1 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: av1C chroma subsampling (0, 1) is not expressible",
            ));
        }
        if monochrome && (chroma_subsampling_x, chroma_subsampling_y) != (1, 1) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: av1C monochrome requires chroma subsampling (1, 1)",
            ));
        }
        // reserved(3) | initial_presentation_delay_present(1) | delay_minus_one(4) or reserved(4)
        let initial_presentation_delay_minus_one = (b3 & 0x10 != 0).then_some(b3 & 0x0f);
        // configOBUs SHALL each carry a size field (§2.3.4); the split also bounds-checks them.
        for obu in iter_obus(config_obus) {
            if !obu?.header.has_size_field {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: av1C configOBUs must carry size fields",
                ));
            }
        }
        Ok(Self {
            seq_profile,
            seq_level_idx_0,
            seq_tier_0,
            high_bitdepth,
            twelve_bit,
            monochrome,
            chroma_subsampling_x,
            chroma_subsampling_y,
            chroma_sample_position,
            initial_presentation_delay_minus_one,
            config_obus: config_obus.to_vec(),
        })
    }

    /// The sample bit depth — 8, 10, or 12 — from `seq_profile`/`high_bitdepth`/`twelve_bit`
    /// (the AV1 `color_config` mapping, AV1 §5.5.2): profile 2 with `high_bitdepth` selects 10 or
    /// 12 bits via `twelve_bit`; otherwise `high_bitdepth` selects 8 or 10 bits.
    #[must_use]
    pub fn bit_depth(&self) -> u8 {
        match (self.seq_profile, self.high_bitdepth, self.twelve_bit) {
            (2, true, true) => 12,
            (_, true, _) => 10,
            _ => 8,
        }
    }

    /// The chroma sampling format from `monochrome` and the subsampling pair (AV1 §6.4.2).
    /// Infallible: the one inexpressible pair, `(0, 1)`, is rejected at [`parse`](Self::parse)
    /// time.
    #[must_use]
    pub fn chroma_format(&self) -> ChromaFormat {
        if self.monochrome {
            return ChromaFormat::Monochrome;
        }
        match (self.chroma_subsampling_x, self.chroma_subsampling_y) {
            (1, 1) => ChromaFormat::Yuv420,
            (1, 0) => ChromaFormat::Yuv422,
            // (0, 1) is rejected at parse, so the only remaining pair is (0, 0).
            _ => ChromaFormat::Yuv444,
        }
    }

    /// Parses the AV1 sequence header (AV1 §5.5) that governs an `av01` item `payload` under this
    /// record, through `gamut-av1`'s [`SequenceHeader::parse`] — the one §5.5 parser in the
    /// workspace, so the container and the codec never disagree about a field.
    ///
    /// The header is taken from the **payload** when it carries one (AVIF v1.2.0 §2.1 requires
    /// exactly one there), and otherwise from [`config_obus`](Self::config_obus), the only other
    /// place AV1-ISOBMFF §2.3.3 lets it live. The payload wins because it is what a decoder reads
    /// last before the frame; §2.3.4 requires the two to match, and that cross-check is not made
    /// here. Only the first sequence header OBU of each source is read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if neither the payload nor `configOBUs` carries a sequence
    /// header OBU, propagates the OBU-split errors of [`crate::iter_obus`] for either source, and
    /// propagates [`SequenceHeader::parse`]'s errors for a malformed header.
    pub fn sequence_header(&self, payload: &[u8]) -> Result<SequenceHeader> {
        let obu = match first_sequence_header(payload)? {
            Some(obu) => obu,
            None => first_sequence_header(&self.config_obus)?.ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: neither the item payload nor av1C configOBUs carries a sequence header",
                )
            })?,
        };
        SequenceHeader::parse(obu)
    }

    /// Converts an `av01` item `payload` to a self-contained low-overhead temporal unit, appending
    /// to `out`: a temporal-delimiter OBU, then the record's [`config_obus`](Self::config_obus),
    /// then every payload OBU — the stream shape a Section-5 AV1 decoder (dav1d, a hardware block)
    /// consumes directly. Bytes are appended, so callers can reuse a scratch buffer
    /// (allocation-conscious).
    ///
    /// A final payload OBU without a size field (which AV1-ISOBMFF §2.4 permits) is re-emitted
    /// *with* one, since decoders consuming a self-contained stream expect every OBU sized; all
    /// other OBUs are copied verbatim. This does **not** de-duplicate a sequence header present in
    /// both `configOBUs` and the payload — an AV1 decoder accepts a repeated sequence header, so
    /// de-duplication is neither required nor performed here.
    ///
    /// # Errors
    ///
    /// Propagates the payload-split errors of [`crate::iter_obus`] (truncated header, size field,
    /// or body). On error, bytes already appended to `out` are left in place.
    pub fn full_stream(&self, payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
        // OBU_TEMPORAL_DELIMITER with a size field and an empty payload: header 0x12, size 0.
        out.extend_from_slice(&[0x12, 0x00]);
        out.extend_from_slice(&self.config_obus);
        for obu in iter_obus(payload) {
            let obu = obu?;
            if obu.header.has_size_field {
                out.extend_from_slice(obu.raw);
            } else {
                // Re-emit with obu_has_size_field set: header byte(s) verbatim except the size
                // bit (0x02), then the minimal leb128 payload size, then the payload.
                let header_len = obu.raw.len() - obu.payload.len();
                // `+ 0x02`, not `| 0x02`: this arm is reached only when `has_size_field` is
                // false, and 0x02 is that very bit, so it is known clear and `|`/`^` agree (#110).
                out.push(obu.raw[0] + 0x02);
                out.extend_from_slice(&obu.raw[1..header_len]);
                write_leb128(obu.payload.len() as u64, out);
                out.extend_from_slice(obu.payload);
            }
        }
        Ok(())
    }

    /// Validates that an `av01` item `payload` satisfies the AVIF still-image constraints — the
    /// AV1 Image Item Data rules of AVIF v1.2.0 §2.1 plus the sync-sample rules of AV1-ISOBMFF
    /// §2.4 it incorporates — so the item is independently decodable:
    ///
    /// - the payload splits exactly into low-overhead OBUs (every OBU carries a size field except,
    ///   optionally, the last — [`crate::iter_obus`]'s contract);
    /// - no `OBU_TILE_LIST` is present (SHALL NOT, §2.4);
    /// - exactly one sequence header OBU is present (§2.1), before the first frame-bearing OBU
    ///   (§2.4 — other OBU types such as metadata may precede it);
    /// - the first frame is a shown key frame (§2.4), checked from the fixed leading header bits:
    ///   a sequence header with `reduced_still_picture_header = 1` implies it (AV1 §5.9.2);
    ///   otherwise the first frame-bearing OBU must be a frame (not a bare tile group) whose
    ///   header starts `show_existing_frame = 0`, `frame_type = KEY_FRAME`, `show_frame = 1`.
    ///
    /// SHOULD-level shapes are accepted without complaint: temporal-delimiter / padding /
    /// redundant-frame-header OBUs present, a sequence header also in
    /// [`config_obus`](Self::config_obus), and `still_picture` /
    /// `reduced_still_picture_header` = 0 (NOTE-level flags). The §2.3.4 rule that a `configOBUs`
    /// sequence header match the payload's is a decoder concern and is not cross-checked here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if any constraint above fails, and propagates the
    /// payload-split ([`crate::iter_obus`]) errors.
    pub fn validate_still_payload(&self, payload: &[u8]) -> Result<()> {
        let mut sequence_headers = 0usize;
        let mut reduced_still_picture = false;
        let mut first_frame_checked = false;
        for obu in iter_obus(payload) {
            let obu = obu?;
            match obu.header.obu_type {
                ObuType::TileList => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "AVIF: item payload must not contain a tile list OBU",
                    ));
                }
                ObuType::SequenceHeader => {
                    sequence_headers += 1;
                    // seq_profile(3) | still_picture(1) | reduced_still_picture_header(1) | …
                    let &[b0, ..] = obu.payload else {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "AVIF: empty sequence header OBU",
                        ));
                    };
                    reduced_still_picture = b0 & 0x08 != 0;
                }
                ty if ty.is_frame_bearing() && !first_frame_checked => {
                    if sequence_headers == 0 {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "AVIF: sequence header OBU must precede the first frame",
                        ));
                    }
                    // A tile group before any frame header is malformed whatever the sequence
                    // header says.
                    if ty == ObuType::TileGroup {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "AVIF: tile group OBU precedes the first frame header",
                        ));
                    }
                    // With reduced_still_picture_header the frame is a shown key frame by
                    // construction (AV1 §5.9.2); otherwise peek the fixed uncompressed-header bits:
                    // show_existing_frame(1) | frame_type(2) | show_frame(1) | …
                    if !reduced_still_picture {
                        let &[b0, ..] = obu.payload else {
                            return Err(Error::invalid_input(
                                env!("CARGO_PKG_NAME"),
                                "AVIF: empty frame OBU",
                            ));
                        };
                        if b0 & 0x80 != 0 {
                            return Err(Error::invalid_input(
                                env!("CARGO_PKG_NAME"),
                                "AVIF: first frame must not be show_existing_frame",
                            ));
                        }
                        if (b0 >> 5) & 0x03 != 0 {
                            return Err(Error::invalid_input(
                                env!("CARGO_PKG_NAME"),
                                "AVIF: first frame must be a key frame",
                            ));
                        }
                        if b0 & 0x10 == 0 {
                            return Err(Error::invalid_input(
                                env!("CARGO_PKG_NAME"),
                                "AVIF: first frame must have show_frame set",
                            ));
                        }
                    }
                    first_frame_checked = true;
                }
                _ => {}
            }
        }
        if sequence_headers != 1 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: item payload must have exactly one sequence header OBU",
            ));
        }
        if !first_frame_checked {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: item payload has no frame-bearing OBU",
            ));
        }
        Ok(())
    }
}

/// The payload of the first sequence header OBU in a low-overhead OBU `stream`, or `None` when it
/// has none. Stops at the first sequence header, so a split error *after* it is not reported.
pub(crate) fn first_sequence_header(stream: &[u8]) -> Result<Option<&[u8]>> {
    for obu in iter_obus(stream) {
        let obu = obu?;
        if obu.header.obu_type == ObuType::SequenceHeader {
            return Ok(Some(obu.payload));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use gamut_av1::Av1StillConfig;

    use super::*;

    #[test]
    fn chroma_format_maps_onto_the_shared_subsampling_vocabulary() {
        // Total mapping, asserted per variant so a mutant that collapses two arms dies. The two
        // enums deliberately do not share a numbering: these discriminants are the codec's
        // `chroma_format_idc`, `ChromaSubsampling`'s are gamut's own.
        assert_eq!(
            ChromaFormat::Monochrome.subsampling(),
            ChromaSubsampling::Cs400
        );
        assert_eq!(ChromaFormat::Yuv420.subsampling(), ChromaSubsampling::Cs420);
        assert_eq!(ChromaFormat::Yuv422.subsampling(), ChromaSubsampling::Cs422);
        assert_eq!(ChromaFormat::Yuv444.subsampling(), ChromaSubsampling::Cs444);
        // The delegation preserves the documented ceiling-division behaviour on odd dimensions,
        // which is the property downstream plane sizing depends on.
        assert_eq!(ChromaFormat::Yuv420.chroma_dimensions(17, 13), (9, 7));
        assert_eq!(ChromaFormat::Yuv422.chroma_dimensions(17, 13), (9, 13));
        assert_eq!(ChromaFormat::Yuv444.chroma_dimensions(17, 13), (17, 13));
        assert_eq!(ChromaFormat::Monochrome.chroma_dimensions(17, 13), (0, 0));
    }
    use crate::encoder::av1c_record;

    #[test]
    fn parse_round_trips_the_encoders_record() {
        // Distinct, non-zero values in every field the writer packs (a 4:2:2 12-bit professional
        // profile), so a swapped shift or mask on either side breaks the round trip.
        let c = Av1StillConfig {
            seq_profile: 2,
            seq_level_idx_0: 0x13,
            seq_tier_0: 1,
            high_bitdepth: true,
            twelve_bit: true,
            monochrome: false,
            chroma_subsampling_x: 1,
            chroma_subsampling_y: 0,
            chroma_sample_position: 0,
            // colr fields are irrelevant to av1C but needed to build the config.
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            full_range: true,
        };
        let parsed = Av1Config::parse(&av1c_record(&c)).unwrap();
        assert_eq!(parsed.seq_profile, c.seq_profile);
        assert_eq!(parsed.seq_level_idx_0, c.seq_level_idx_0);
        assert_eq!(parsed.seq_tier_0, c.seq_tier_0);
        assert_eq!(parsed.high_bitdepth, c.high_bitdepth);
        assert_eq!(parsed.twelve_bit, c.twelve_bit);
        assert_eq!(parsed.monochrome, c.monochrome);
        assert_eq!(parsed.chroma_subsampling_x, c.chroma_subsampling_x);
        assert_eq!(parsed.chroma_subsampling_y, c.chroma_subsampling_y);
        assert_eq!(parsed.chroma_sample_position, c.chroma_sample_position);
        assert_eq!(parsed.initial_presentation_delay_minus_one, None);
        assert!(parsed.config_obus.is_empty());
        assert_eq!(parsed.bit_depth(), 12);
        assert_eq!(parsed.chroma_format(), ChromaFormat::Yuv422);
    }
}
