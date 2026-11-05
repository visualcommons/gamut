//! `cicpType` (ICC.1:2022 §10.3): coding-independent code points for video signal type
//! identification, added in ICC.1:2022 to let HDR/wide-gamut signalling ride in a profile.
//!
//! The four fields are ITU-T H.273 (ISO/IEC 23091-2) code points, carried in the workspace's one
//! [`Cicp`] type. gamut-icc records the raw code points only; mapping them to concrete
//! primaries/transfer functions is the job of [`gamut_color`](https://docs.rs/gamut-color).

use gamut_core::Cicp;

use crate::bytes::ByteReader;
use crate::error::{IccError, Result};

/// Decodes a `cicpType` element.
///
/// §10.3 encodes each field as a `uInt8Number` "as specified in Recommendation ITU-T H.273", and
/// H.273 §8.3 defines `VideoFullRangeFlag` as a single bit, so a flag byte other than 0 or 1 is
/// rejected. That is the one value this reader stops accepting by decoding into [`Cicp`]'s
/// `bool`: an element whose byte 11 is 2–255, which no conformant writer emits and which could
/// only be kept by giving up `Cicp`'s flag type.
///
/// # Errors
///
/// [`IccError::Malformed`] if the element is shorter than 12 bytes or its flag byte is not 0 or 1.
pub(crate) fn decode_cicp(element: &[u8]) -> Result<Cicp> {
    let mut r = ByteReader::at(element, 8)?;
    let colour_primaries = u16::from(r.u8()?);
    let transfer_characteristics = u16::from(r.u8()?);
    let matrix_coefficients = u16::from(r.u8()?);
    let full_range = match r.u8()? {
        0 => false,
        1 => true,
        _ => {
            return Err(IccError::Malformed(
                "icc: cicp VideoFullRangeFlag is neither 0 nor 1",
            ));
        }
    };
    Ok(Cicp {
        colour_primaries,
        transfer_characteristics,
        matrix_coefficients,
        full_range,
    })
}

/// Serializes a `cicpType` element (the inverse of [`decode_cicp`]).
///
/// # Errors
///
/// [`IccError::Malformed`] if a code point exceeds 255: [`Cicp`] holds `u16` code points for the
/// carriers that have the room, but §10.3 stores each in one `uInt8Number`.
pub(crate) fn encode_cicp(cicp: &Cicp, out: &mut Vec<u8>) -> Result<()> {
    let byte = |code: u16| {
        u8::try_from(code).map_err(|_| IccError::Malformed("icc: cicp code point exceeds 255"))
    };
    let fields = [
        byte(cicp.colour_primaries)?,
        byte(cicp.transfer_characteristics)?,
        byte(cicp.matrix_coefficients)?,
        u8::from(cicp.full_range),
    ];
    out.extend_from_slice(b"cicp");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&fields);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `cicpType` element carrying `fields` (bytes 8–11).
    fn element(fields: [u8; 4]) -> Vec<u8> {
        let mut e = b"cicp\x00\x00\x00\x00".to_vec();
        e.extend_from_slice(&fields);
        e
    }

    #[test]
    fn decodes_srgb_code_points() {
        // 1-13-0-1 — the IEC 61966-2-1 sRGB full-range RGB encoding (§10.3 examples).
        let cicp = decode_cicp(&element([1, 13, 0, 1])).unwrap();
        assert_eq!(
            cicp,
            Cicp {
                colour_primaries: 1,
                transfer_characteristics: 13,
                matrix_coefficients: 0,
                full_range: true,
            }
        );
    }

    #[test]
    fn decodes_a_narrow_range_flag() {
        // 9-16-0-0 — PQ BT.2100 narrow-range R'G'B' (§10.3 examples).
        assert!(!decode_cicp(&element([9, 16, 0, 0])).unwrap().full_range);
    }

    #[test]
    fn rejects_a_flag_byte_that_is_not_a_bit() {
        let err = decode_cicp(&element([1, 13, 0, 2])).unwrap_err();
        assert_eq!(
            err.message(),
            "icc: cicp VideoFullRangeFlag is neither 0 nor 1"
        );
    }

    #[test]
    fn encodes_the_exact_bytes() {
        // 9-16-0-1 — PQ BT.2100 full-range R'G'B'.
        let cicp = Cicp {
            colour_primaries: 9,
            transfer_characteristics: 16,
            matrix_coefficients: 0,
            full_range: true,
        };
        let mut out = Vec::new();
        encode_cicp(&cicp, &mut out).unwrap();
        assert_eq!(out, element([9, 16, 0, 1]));
        assert_eq!(decode_cicp(&out).unwrap(), cicp);
    }

    #[test]
    fn refuses_a_code_point_over_255_in_each_field() {
        let max = Cicp {
            colour_primaries: 255,
            transfer_characteristics: 255,
            matrix_coefficients: 255,
            full_range: false,
        };
        let mut out = Vec::new();
        encode_cicp(&max, &mut out).unwrap();
        assert_eq!(out, element([255, 255, 255, 0]));
        for wide in [
            Cicp {
                colour_primaries: 256,
                ..max
            },
            Cicp {
                transfer_characteristics: 256,
                ..max
            },
            Cicp {
                matrix_coefficients: 256,
                ..max
            },
        ] {
            let err = encode_cicp(&wide, &mut Vec::new()).unwrap_err();
            assert_eq!(err.message(), "icc: cicp code point exceeds 255");
        }
    }

    #[test]
    fn rejects_truncated_element() {
        assert!(decode_cicp(b"cicp\x00\x00\x00\x00\x01\x0d").is_err());
    }
}
