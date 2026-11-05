//! JPEG marker codes (T.81 Table B.1) and the fixed-structure marker-segment writers (§B.2).
//!
//! Every marker is the two bytes `0xFF, code`. A *marker segment* (§B.1.1.4) is a marker followed by
//! a two-byte big-endian length that counts itself but not the marker. This module owns the marker
//! constants and the segments whose layout is fully determined by their parameters — SOI/EOI, the
//! JFIF APP0 segment (T.871 §10.1), the SOF0 (baseline) and SOF2 (progressive) frame headers
//! (§B.2.2), the SOS scan header (§B.2.3, baseline and progressive band forms), DRI (§B.2.4.4), and
//! the RSTn restart markers (§B.2.1). The DQT and DHT segments live with the table data they
//! serialize ([`crate::quant`] and [`crate::huffman`]).

/// Marker codes from T.81 Table B.1. Each is the second byte of the two-byte `0xFF, code` marker.
pub mod code {
    /// Start of image.
    pub const SOI: u8 = 0xD8;
    /// End of image.
    pub const EOI: u8 = 0xD9;
    /// Baseline DCT, Huffman coding, start of frame.
    pub const SOF0: u8 = 0xC0;
    /// Extended sequential DCT, Huffman coding, start of frame. Treated identically to [`SOF0`] at
    /// 8-bit precision by the decoder (the extended process only differs at 12-bit).
    pub const SOF1: u8 = 0xC1;
    /// Progressive DCT, Huffman coding, start of frame.
    pub const SOF2: u8 = 0xC2;
    /// Lossless (sequential) process, start of frame.
    pub const SOF3: u8 = 0xC3;
    /// Define Huffman table(s).
    pub const DHT: u8 = 0xC4;
    /// Define arithmetic coding conditioning(s).
    pub const DAC: u8 = 0xCC;
    /// Define quantization table(s).
    pub const DQT: u8 = 0xDB;
    /// Define restart interval.
    pub const DRI: u8 = 0xDD;
    /// Define number of lines (§B.2.5): supplies the frame height when the SOF `Y` field was 0.
    pub const DNL: u8 = 0xDC;
    /// Start of scan.
    pub const SOS: u8 = 0xDA;
    /// Application data segment 0 (used by JFIF).
    pub const APP0: u8 = 0xE0;
    /// Application data segment 1 (used by EXIF, Exif 3.0 §4.7.2, and XMP, XMP Part 3 §1.1.3).
    pub const APP1: u8 = 0xE1;
    /// Application data segment 2 (used for `ICC_PROFILE` chunks, ICC.1:2001-04 Annex B.4).
    pub const APP2: u8 = 0xE2;
    /// Application data segment 14 (used by Adobe for the colour-transform flag, TN #5116).
    pub const APP14: u8 = 0xEE;
    /// End of image.
    pub const EOI_CODE: u8 = 0xD9;
    /// Temporary marker for arithmetic coding (`0x01`); a standalone marker with no segment.
    pub const TEM: u8 = 0x01;
    /// First restart marker (RST0); RSTm is `RST0 + (m & 7)`, i.e. `0xD0..=0xD7`.
    pub const RST0: u8 = 0xD0;
    /// Last restart marker (RST7).
    pub const RST7: u8 = 0xD7;
}

/// Packs two 4-bit fields into one byte, `hi` in the upper nibble and `lo` in the lower — the
/// recurring two-parameters-per-byte convention of the Annex B tables (`Hi|Vi` in SOF §B.2.2,
/// `Tdj|Taj` in SOS §B.2.3, `Tc|Th` in DHT §B.2.4.2, `Pq|Tq` in DQT §B.2.4.1) and of the
/// `RRRRSSSS` run/size symbols of §F.1.2.2.
///
/// Composed with `+` rather than `|`: after `hi << 4` the low four bits are zero, so adding the
/// masked `lo` occupies exactly those vacant bits (`hi` is a 4-bit field at every call site).
pub fn pack_nibbles(hi: u8, lo: u8) -> u8 {
    (hi << 4) + (lo & 0x0F)
}

/// Writes a two-byte marker (`0xFF, code`) with no parameters, e.g. SOI, EOI, RSTn.
pub fn write_marker(out: &mut Vec<u8>, code: u8) {
    out.push(0xFF);
    out.push(code);
}

/// Writes a marker and the two-byte big-endian segment length that follows it. `len` is the full
/// segment length **including** the two length bytes (§B.1.1.4); callers then append `len - 2`
/// payload bytes.
///
/// # Panics
///
/// Panics in debug builds if `len` exceeds `u16::MAX` (a marker segment cannot encode a longer
/// length); the encoder never constructs one this large.
pub fn write_segment_header(out: &mut Vec<u8>, code: u8, len: usize) {
    debug_assert!(
        len <= usize::from(u16::MAX),
        "marker segment length overflows u16"
    );
    write_marker(out, code);
    out.extend_from_slice(&(len as u16).to_be_bytes());
}

/// The physical-density unit of the JFIF APP0 segment (T.871 §10.1, `units` field).
///
/// `#[repr(u8)]` with each discriminant the `units` code T.871 assigns, so the value written is
/// the discriminant itself. Not `#[non_exhaustive]`: T.871 §10.1 defines exactly these three codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DensityUnit {
    /// `units = 0`: no absolute unit; the densities express only the pixel aspect ratio.
    AspectRatio = 0,
    /// `units = 1`: dots per inch.
    Dpi = 1,
    /// `units = 2`: dots per centimetre.
    Dpcm = 2,
}

/// Appends the JFIF APP0 marker segment (T.871 §10.1), no thumbnail (`Lp = 16`). Version is 1.02
/// (`0x0102`) as mandated by T.871. `x_density`/`y_density` must be non-zero (the caller enforces
/// this); the fields are written big-endian.
pub fn write_app0_jfif(out: &mut Vec<u8>, unit: DensityUnit, x_density: u16, y_density: u16) {
    // Lp = 2 (length) + 5 (identifier) + 2 (version) + 1 (units) + 2 + 2 (densities) + 1 + 1 (no
    // thumbnail) = 16.
    write_segment_header(out, code::APP0, 16);
    out.extend_from_slice(b"JFIF\0"); // identifier, zero-terminated (T.50/ISO 646)
    out.extend_from_slice(&[0x01, 0x02]); // version 1.02
    out.push(unit as u8);
    out.extend_from_slice(&x_density.to_be_bytes());
    out.extend_from_slice(&y_density.to_be_bytes());
    out.push(0); // thumbnail width = 0
    out.push(0); // thumbnail height = 0
}

/// Appends the Adobe APP14 marker segment (Adobe TN #5116, `Lp = 14`): the `"Adobe"` identifier,
/// DCTEncode version 100, zero flag words, and the colour-`transform` byte (0 = untransformed
/// RGB/CMYK, 1 = YCbCr, 2 = YCCK). The encoder writes it (with `transform = 0`) for streams whose
/// three components are **not** YCbCr, so a decoder never guesses a YCbCr inverse.
pub fn write_app14_adobe(out: &mut Vec<u8>, transform: u8) {
    // Lp = 2 (length) + 5 (identifier) + 2 (version) + 2 + 2 (flags0/flags1) + 1 (transform) = 14.
    write_segment_header(out, code::APP14, 14);
    out.extend_from_slice(b"Adobe");
    out.extend_from_slice(&[0x00, 0x64]); // DCTEncode version 100
    out.extend_from_slice(&[0x00, 0x00]); // flags0
    out.extend_from_slice(&[0x00, 0x00]); // flags1
    out.push(transform);
}

/// Appends the baseline SOF0 frame header (§B.2.2): precision 8, image `height`×`width`, and one
/// entry per component as `(Ci, Hi, Vi, Tqi)` — the component id, horizontal/vertical sampling
/// factors, and quantization-table destination.
pub fn write_sof0(out: &mut Vec<u8>, width: u16, height: u16, components: &[(u8, u8, u8, u8)]) {
    // Lf = 8 + 3·Nf (§B.2.2, Table B.2).
    let len = 8 + 3 * components.len();
    write_segment_header(out, code::SOF0, len);
    out.push(8); // P: sample precision (bits) — baseline is 8
    out.extend_from_slice(&height.to_be_bytes()); // Y: number of lines
    out.extend_from_slice(&width.to_be_bytes()); // X: samples per line
    out.push(components.len() as u8); // Nf
    for &(id, h, v, tq) in components {
        out.push(id);
        out.push(pack_nibbles(h, v)); // Hi (high nibble) | Vi (low nibble)
        out.push(tq);
    }
}

/// Appends the progressive SOF2 frame header (§B.2.2): identical layout to [`write_sof0`] but with
/// the SOF2 marker (progressive DCT, Huffman coding). Precision 8, image `height`×`width`, and one
/// `(Ci, Hi, Vi, Tqi)` entry per component.
pub fn write_sof2(out: &mut Vec<u8>, width: u16, height: u16, components: &[(u8, u8, u8, u8)]) {
    // Lf = 8 + 3·Nf (§B.2.2, Table B.2).
    let len = 8 + 3 * components.len();
    write_segment_header(out, code::SOF2, len);
    out.push(8); // P: sample precision (bits) — 8-bit progressive
    out.extend_from_slice(&height.to_be_bytes()); // Y: number of lines
    out.extend_from_slice(&width.to_be_bytes()); // X: samples per line
    out.push(components.len() as u8); // Nf
    for &(id, h, v, tq) in components {
        out.push(id);
        out.push(pack_nibbles(h, v)); // Hi (high nibble) | Vi (low nibble)
        out.push(tq);
    }
}

/// Appends the SOS scan header (§B.2.3) for a baseline scan: one entry per component as
/// `(Csj, Tdj, Taj)` (component selector, DC and AC entropy-table destinations), with the baseline
/// spectral-selection/point-transform fields fixed to `Ss = 0`, `Se = 63`, `Ah = Al = 0`.
pub fn write_sos(out: &mut Vec<u8>, components: &[(u8, u8, u8)]) {
    write_sos_bands(out, components, 0, 63, 0, 0);
}

/// Appends the SOS scan header (§B.2.3) with explicit spectral-selection (`Ss`, `Se`) and
/// successive-approximation (`Ah`, `Al`) fields — the general form used by progressive (SOF2) scans.
/// Each component is `(Csj, Tdj, Taj)`. [`write_sos`] is this with the baseline band `(0, 63, 0, 0)`.
pub fn write_sos_bands(
    out: &mut Vec<u8>,
    components: &[(u8, u8, u8)],
    ss: u8,
    se: u8,
    ah: u8,
    al: u8,
) {
    // Ls = 6 + 2·Ns (§B.2.3, Table B.3).
    let len = 6 + 2 * components.len();
    write_segment_header(out, code::SOS, len);
    out.push(components.len() as u8); // Ns
    for &(cs, td, ta) in components {
        out.push(cs);
        out.push(pack_nibbles(td, ta)); // Tdj (high nibble) | Taj (low nibble)
    }
    out.push(ss); // Ss: start of spectral selection
    out.push(se); // Se: end of spectral selection
    out.push(pack_nibbles(ah, al)); // Ah (high nibble) | Al (low nibble)
}

/// Appends a DRI segment (§B.2.4.4) declaring a restart interval of `interval` MCUs (`Lr = 4`).
pub fn write_dri(out: &mut Vec<u8>, interval: u16) {
    write_segment_header(out, code::DRI, 4);
    out.extend_from_slice(&interval.to_be_bytes()); // Ri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_nibbles_places_each_field() {
        // Asymmetric values so a nibble swap, a shift-direction flip, or a +/− compose error each
        // produce a distinct wrong byte: 2·16 + 5 = 0x25, not 0x52 / 0x07 / 0x1B / 0x0A.
        assert_eq!(pack_nibbles(2, 5), 0x25);
        assert_eq!(pack_nibbles(0, 0xF), 0x0F);
        assert_eq!(pack_nibbles(0xF, 0), 0xF0);
        // The low argument is masked to 4 bits (0x1F & 0x0F = 0xF), the high one occupies the top.
        assert_eq!(pack_nibbles(1, 0x1F), 0x1F);
    }

    #[test]
    fn segment_header_marker_and_length_bytes() {
        // The length is big-endian and counts itself; DRI's Lr is 4.
        let mut out = Vec::new();
        write_segment_header(&mut out, code::DRI, 4);
        assert_eq!(out, vec![0xFF, 0xDD, 0x00, 0x04]);
    }

    #[test]
    fn app14_is_the_canonical_14_byte_adobe_segment() {
        // Byte-exact Adobe APP14 (TN #5116): marker, Lp=14, "Adobe", version 100, zero flags,
        // transform byte last.
        let mut out = Vec::new();
        write_app14_adobe(&mut out, 0);
        assert_eq!(
            out,
            vec![
                0xFF, 0xEE, // APP14
                0x00, 0x0E, // Lp = 14
                0x41, 0x64, 0x6F, 0x62, 0x65, // "Adobe"
                0x00, 0x64, // DCTEncode version 100
                0x00, 0x00, // flags0
                0x00, 0x00, // flags1
                0x00, // transform = 0 (untransformed)
            ]
        );
        // The transform byte is the caller's, verbatim.
        let mut ycc = Vec::new();
        write_app14_adobe(&mut ycc, 1);
        assert_eq!(ycc[15], 1);
    }

    #[test]
    fn app0_is_the_canonical_16_byte_jfif_segment() {
        // Byte-exact JFIF APP0: marker, Lp=16, "JFIF\0", version 1.02, units, densities, no thumbnail.
        let mut out = Vec::new();
        write_app0_jfif(&mut out, DensityUnit::Dpi, 72, 72);
        assert_eq!(
            out,
            vec![
                0xFF, 0xE0, // APP0
                0x00, 0x10, // Lp = 16
                0x4A, 0x46, 0x49, 0x46, 0x00, // "JFIF\0"
                0x01, 0x02, // version 1.02
                0x01, // units = dpi
                0x00, 0x48, 0x00, 0x48, // 72 x 72
                0x00, 0x00, // no thumbnail
            ]
        );
    }

    #[test]
    fn app0_unit_codes() {
        let mut a = Vec::new();
        write_app0_jfif(&mut a, DensityUnit::AspectRatio, 1, 1);
        assert_eq!(a[11], 0); // units byte
        let mut b = Vec::new();
        write_app0_jfif(&mut b, DensityUnit::Dpcm, 1, 1);
        assert_eq!(b[11], 2);
    }

    #[test]
    fn sof0_encodes_dimensions_and_sampling() {
        // Colour SOF0: 3 components, so Lf = 8 + 9 = 17; precision 8; height then width big-endian;
        // Y uses 2×2 sampling (4:2:0), chroma 1×1.
        let mut out = Vec::new();
        write_sof0(
            &mut out,
            640,
            480,
            &[(1, 2, 2, 0), (2, 1, 1, 1), (3, 1, 1, 1)],
        );
        assert_eq!(&out[..2], &[0xFF, 0xC0]);
        assert_eq!(&out[2..4], &[0x00, 17]); // Lf
        assert_eq!(out[4], 8); // P
        assert_eq!(&out[5..7], &480u16.to_be_bytes()); // Y = height
        assert_eq!(&out[7..9], &640u16.to_be_bytes()); // X = width
        assert_eq!(out[9], 3); // Nf
        assert_eq!(&out[10..13], &[1, 0x22, 0]); // comp 1: id, H<<4|V, Tq
        assert_eq!(&out[13..16], &[2, 0x11, 1]);
        assert_eq!(&out[16..19], &[3, 0x11, 1]);
        assert_eq!(out.len(), 2 + 17);
    }

    #[test]
    fn sos_baseline_spectral_fields_fixed() {
        // Grayscale SOS: 1 component, Ls = 6 + 2 = 8; Ss=0, Se=63, Ah=Al=0.
        let mut out = Vec::new();
        write_sos(&mut out, &[(1, 0, 0)]);
        assert_eq!(&out[..2], &[0xFF, 0xDA]);
        assert_eq!(&out[2..4], &[0x00, 8]); // Ls
        assert_eq!(out[4], 1); // Ns
        assert_eq!(&out[5..7], &[1, 0x00]); // Cs=1, Td<<4|Ta=0
        assert_eq!(&out[7..10], &[0, 63, 0]); // Ss, Se, Ah|Al
        // Colour SOS pins the Td/Ta nibble packing: Cb/Cr use DC/AC table 1 → 0x11.
        let mut c = Vec::new();
        write_sos(&mut c, &[(1, 0, 0), (2, 1, 1), (3, 1, 1)]);
        assert_eq!(&c[2..4], &[0x00, 12]); // Ls = 6 + 6
        assert_eq!(c[5 + 1], 0x00); // Y: Td<<4|Ta
        assert_eq!(c[7 + 1], 0x11); // Cb
        assert_eq!(c[9 + 1], 0x11); // Cr
    }

    #[test]
    fn sof2_uses_the_progressive_marker() {
        // Same layout as SOF0 but the marker byte is 0xC2 (progressive DCT).
        let mut out = Vec::new();
        write_sof2(
            &mut out,
            640,
            480,
            &[(1, 2, 2, 0), (2, 1, 1, 1), (3, 1, 1, 1)],
        );
        assert_eq!(&out[..2], &[0xFF, 0xC2]);
        assert_eq!(&out[2..4], &[0x00, 17]); // Lf = 8 + 9
        assert_eq!(out[4], 8); // P
        assert_eq!(&out[5..7], &480u16.to_be_bytes()); // Y
        assert_eq!(&out[7..9], &640u16.to_be_bytes()); // X
        assert_eq!(&out[10..13], &[1, 0x22, 0]);
    }

    #[test]
    fn sos_bands_encodes_spectral_and_approximation_fields() {
        // A progressive AC refinement band: Ss=1, Se=63, Ah=2, Al=1 → last three bytes 1, 63, 0x21.
        let mut out = Vec::new();
        write_sos_bands(&mut out, &[(1, 0, 0)], 1, 63, 2, 1);
        assert_eq!(&out[..4], &[0xFF, 0xDA, 0x00, 8]); // marker + Ls
        assert_eq!(out[4], 1); // Ns
        assert_eq!(&out[5..7], &[1, 0x00]); // Cs, Td|Ta
        assert_eq!(&out[7..10], &[1, 63, 0x21]); // Ss, Se, Ah|Al
        // write_sos is the baseline band (0, 63, 0, 0): last three bytes 0, 63, 0.
        let mut base = Vec::new();
        write_sos(&mut base, &[(1, 0, 0)]);
        assert_eq!(&base[7..10], &[0, 63, 0]);
    }

    #[test]
    fn plain_markers_are_two_bytes() {
        let mut out = Vec::new();
        write_marker(&mut out, code::SOI);
        write_marker(&mut out, code::EOI);
        assert_eq!(out, vec![0xFF, 0xD8, 0xFF, 0xD9]);
        // RSTm cycles 0xD0..=0xD7.
        let mut r = Vec::new();
        write_marker(&mut r, code::RST0 + 7);
        assert_eq!(r, vec![0xFF, 0xD7]);
    }
}
