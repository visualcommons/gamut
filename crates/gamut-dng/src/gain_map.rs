//! Typed `ProfileGainTableMap` (52525, DNG 1.6) and `ProfileGainTableMap2` (52544, DNG 1.7):
//! spatially varying gain tables applied while rendering, e.g. Apple ProRAW's local tone map.
//!
//! Both tags share one model, [`ProfileGainTableMap`]: a subsampled `MapPointsV × MapPointsH`
//! grid of gain tables with `MapPointsN` entries each, plus the grid's placement (origin/spacing
//! in image-relative coordinates) and the 5-vector of input weights that maps an RGB value to a
//! table index. The v2 tag adds a `Gamma` applied to the table input, and integer gain storage
//! (`DataType`, with `GainMin`/`GainMax` defining the represented range). When both tags are
//! present, v2 supersedes v1 (spec p. 88).
//!
//! Unlike opcode lists (always big-endian), these payloads are stored in the **file's byte
//! order** — parsing and serialisation take the [`ByteOrder`]. Serialisation is byte-exact for
//! any parsed map (nothing is dropped or renormalised), which the Adobe PGTM2 sample files gate.
//!
//! Applying the gain map to rendered pixels is rendering-pipeline work and out of scope here,
//! with one helper exception: [`ProfileGainTableMap::gain_at`] decodes a single stored entry to
//! its floating-point gain.

use gamut_core::{Error, Result};
use gamut_ifd::ByteOrder;

/// The stored gain values of a gain-table map, preserved exactly as encoded.
///
/// The variants mirror the v2 `DataType` values 0–3; a v1 tag always stores [`F32`](Self::F32).
/// Integer variants are code values over the `GainMin ..= GainMax` range; `F16` keeps the raw
/// half-float bit patterns (the crate does not convert them).
#[derive(Debug, Clone, PartialEq)]
pub enum GainValues {
    /// `DataType 0` — unsigned 8-bit codes over `GainMin ..= GainMax`.
    U8(Vec<u8>),
    /// `DataType 1` — unsigned 16-bit codes over `GainMin ..= GainMax`.
    U16(Vec<u16>),
    /// `DataType 2` — IEEE half-float gains, kept as raw bit patterns.
    F16(Vec<u16>),
    /// `DataType 3` — 32-bit float gains (the only representation the v1 tag allows).
    F32(Vec<f32>),
}

impl GainValues {
    /// The number of stored gain entries.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            GainValues::U8(v) => v.len(),
            GainValues::U16(v) | GainValues::F16(v) => v.len(),
            GainValues::F32(v) => v.len(),
        }
    }

    /// Whether no gain entries are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The v2 `DataType` code of this representation.
    fn data_type(&self) -> u32 {
        match self {
            GainValues::U8(_) => 0,
            GainValues::U16(_) => 1,
            GainValues::F16(_) => 2,
            GainValues::F32(_) => 3,
        }
    }
}

/// A parsed `ProfileGainTableMap`/`ProfileGainTableMap2` payload (see the module docs).
///
/// Plain data: every field is public and the struct is literally constructible, since it is both
/// a decoder output ([`DecodedDng`](crate::DecodedDng)) and an encoder input
/// ([`DngEncoder::with_gain_table_map`](crate::DngEncoder::with_gain_table_map)). The layout is
/// frozen by the DNG specification.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileGainTableMap {
    /// `MapPointsV` — gain tables in the vertical direction (≥ 1).
    pub points_v: u32,
    /// `MapPointsH` — gain tables in the horizontal direction (≥ 1).
    pub points_h: u32,
    /// `MapSpacingV` — vertical spacing between tables, relative to the image height.
    pub spacing_v: f64,
    /// `MapSpacingH` — horizontal spacing between tables, relative to the image width.
    pub spacing_h: f64,
    /// `MapOriginV` — vertical origin of the grid, relative to the image height (may be
    /// negative).
    pub origin_v: f64,
    /// `MapOriginH` — horizontal origin of the grid, relative to the image width.
    pub origin_h: f64,
    /// `MapPointsN` — entries per gain table (≥ 1).
    pub points_n: u32,
    /// `MapInputWeights` — the dot-product weights over `(R, G, B, min(R,G,B), max(R,G,B))`
    /// that produce the table input value.
    pub input_weights: [f32; 5],
    /// `Gamma` (v2 only; `1.0` for a v1 tag) — applied to the clamped table input value; must
    /// be within `0.25 ..= 4.0`.
    pub gamma: f32,
    /// `GainMin` (v2 only) — the gain represented by integer code 0. Ignored for float storage
    /// but preserved verbatim for byte-exact round-trips.
    pub gain_min: f32,
    /// `GainMax` (v2 only) — the gain represented by the maximum integer code.
    pub gain_max: f32,
    /// The stored gain entries, `points_v * points_h * points_n` long, in V-major, H, then N
    /// order.
    pub gains: GainValues,
}

impl ProfileGainTableMap {
    /// Parses a `ProfileGainTableMap` (52525) payload: the 64-byte header followed by 32-bit
    /// float gains, in the file's byte order.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the byte count disagrees with the header's dimensions,
    /// a dimension is zero, or a gain is negative or not finite.
    pub fn parse_v1(bytes: &[u8], order: ByteOrder) -> Result<Self> {
        let mut r = Reader {
            bytes,
            pos: 0,
            order,
        };
        let (points_v, points_h, spacing, points_n, input_weights) = r.header()?;
        let count = table_count(points_v, points_h, points_n)?;
        if bytes.len() != 64 + 4 * count {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: ProfileGainTableMap byte count disagrees with its dimensions",
            ));
        }
        let mut gains = Vec::with_capacity(count);
        for _ in 0..count {
            let g = r.f32()?;
            if !g.is_finite() || g < 0.0 {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: ProfileGainTableMap gains must be finite and non-negative",
                ));
            }
            gains.push(g);
        }
        Ok(Self {
            points_v,
            points_h,
            spacing_v: spacing[0],
            spacing_h: spacing[1],
            origin_v: spacing[2],
            origin_h: spacing[3],
            points_n,
            input_weights,
            gamma: 1.0,
            gain_min: 0.0,
            gain_max: 0.0,
            gains: GainValues::F32(gains),
        })
    }

    /// Parses a `ProfileGainTableMap2` (52544) payload: the 80-byte header followed by gains in
    /// the declared `DataType`, in the file's byte order.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the byte count disagrees with the header, a dimension
    /// is zero, `DataType` is unknown, `Gamma` is outside `0.25 ..= 4.0`, or a float gain is not
    /// finite.
    pub fn parse_v2(bytes: &[u8], order: ByteOrder) -> Result<Self> {
        let mut r = Reader {
            bytes,
            pos: 0,
            order,
        };
        let (points_v, points_h, spacing, points_n, input_weights) = r.header()?;
        let data_type = r.u32()?;
        let gamma = r.f32()?;
        let gain_min = r.f32()?;
        let gain_max = r.f32()?;
        if !(0.25..=4.0).contains(&gamma) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: ProfileGainTableMap2 Gamma must be within 0.25 ..= 4.0",
            ));
        }
        let count = table_count(points_v, points_h, points_n)?;
        let entry = match data_type {
            0 => 1,
            1 | 2 => 2,
            3 => 4,
            _ => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: ProfileGainTableMap2 has an unknown DataType",
                ));
            }
        };
        if bytes.len() != 80 + entry * count {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: ProfileGainTableMap2 byte count disagrees with its dimensions",
            ));
        }
        let gains = match data_type {
            0 => GainValues::U8(bytes[r.pos..].to_vec()),
            1 | 2 => {
                let mut v = Vec::with_capacity(count);
                for _ in 0..count {
                    v.push(r.u16()?);
                }
                if data_type == 1 {
                    GainValues::U16(v)
                } else {
                    GainValues::F16(v)
                }
            }
            _ => {
                let mut v = Vec::with_capacity(count);
                for _ in 0..count {
                    let g = r.f32()?;
                    if !g.is_finite() {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "DNG: ProfileGainTableMap2 gains must be finite",
                        ));
                    }
                    v.push(g);
                }
                GainValues::F32(v)
            }
        };
        Ok(Self {
            points_v,
            points_h,
            spacing_v: spacing[0],
            spacing_h: spacing[1],
            origin_v: spacing[2],
            origin_h: spacing[3],
            points_n,
            input_weights,
            gamma,
            gain_min,
            gain_max,
            gains,
        })
    }

    /// Serialises as a `ProfileGainTableMap` (52525) payload in `order`. Byte-exact for a map
    /// parsed with [`Self::parse_v1`].
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the gains are not the 32-bit floats v1 requires, the
    /// gain count disagrees with the dimensions, or `gamma` is not the 1.0 v1 cannot store.
    pub fn to_bytes_v1(&self, order: ByteOrder) -> Result<Vec<u8>> {
        let GainValues::F32(gains) = &self.gains else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: ProfileGainTableMap (v1) stores 32-bit float gains only",
            ));
        };
        if self.gamma != 1.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: ProfileGainTableMap (v1) cannot store a Gamma; use ProfileGainTableMap2",
            ));
        }
        let count = table_count(self.points_v, self.points_h, self.points_n)?;
        if gains.len() != count {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: gain count must be MapPointsV * MapPointsH * MapPointsN",
            ));
        }
        let mut w = Writer {
            out: Vec::new(),
            order,
        };
        self.write_header(&mut w);
        for &g in gains {
            w.f32(g);
        }
        Ok(w.out)
    }

    /// Serialises as a `ProfileGainTableMap2` (52544) payload in `order`. Byte-exact for a map
    /// parsed with [`Self::parse_v2`].
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the gain count disagrees with the dimensions or
    /// `gamma` is outside `0.25 ..= 4.0`.
    pub fn to_bytes_v2(&self, order: ByteOrder) -> Result<Vec<u8>> {
        if !(0.25..=4.0).contains(&self.gamma) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: ProfileGainTableMap2 Gamma must be within 0.25 ..= 4.0",
            ));
        }
        let count = table_count(self.points_v, self.points_h, self.points_n)?;
        if self.gains.len() != count {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: gain count must be MapPointsV * MapPointsH * MapPointsN",
            ));
        }
        let mut w = Writer {
            out: Vec::new(),
            order,
        };
        self.write_header(&mut w);
        w.u32(self.gains.data_type());
        w.f32(self.gamma);
        w.f32(self.gain_min);
        w.f32(self.gain_max);
        match &self.gains {
            GainValues::U8(v) => w.out.extend_from_slice(v),
            GainValues::U16(v) | GainValues::F16(v) => {
                for &x in v {
                    w.u16(x);
                }
            }
            GainValues::F32(v) => {
                for &x in v {
                    w.f32(x);
                }
            }
        }
        Ok(w.out)
    }

    /// The floating-point gain of table `(v, h)` entry `n`, decoding integer storage through
    /// `GainMin`/`GainMax` (half-float storage is not decoded — `None`). `None` when the index
    /// is out of range.
    #[must_use]
    pub fn gain_at(&self, v: u32, h: u32, n: u32) -> Option<f32> {
        if v >= self.points_v || h >= self.points_h || n >= self.points_n {
            return None;
        }
        let index = ((v as usize * self.points_h as usize) + h as usize) * self.points_n as usize
            + n as usize;
        match &self.gains {
            GainValues::U8(codes) => Some(
                self.gain_min
                    + (f32::from(*codes.get(index)?) / 255.0) * (self.gain_max - self.gain_min),
            ),
            GainValues::U16(codes) => Some(
                self.gain_min
                    + (f32::from(*codes.get(index)?) / 65535.0) * (self.gain_max - self.gain_min),
            ),
            GainValues::F16(_) => None,
            GainValues::F32(gains) => gains.get(index).copied(),
        }
    }

    /// Writes the shared 64-byte header prefix.
    fn write_header(&self, w: &mut Writer) {
        w.u32(self.points_v);
        w.u32(self.points_h);
        w.f64(self.spacing_v);
        w.f64(self.spacing_h);
        w.f64(self.origin_v);
        w.f64(self.origin_h);
        w.u32(self.points_n);
        for &weight in &self.input_weights {
            w.f32(weight);
        }
    }
}

/// `MapPointsV * MapPointsH * MapPointsN` with zero and overflow rejection.
fn table_count(points_v: u32, points_h: u32, points_n: u32) -> Result<usize> {
    if points_v == 0 || points_h == 0 || points_n == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: gain-table map dimensions must be non-zero",
        ));
    }
    (points_v as usize)
        .checked_mul(points_h as usize)
        .and_then(|n| n.checked_mul(points_n as usize))
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: gain-table map dimensions overflow",
            )
        })
}

/// A bounds-checked little cursor over the payload in the file's byte order.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    order: ByteOrder,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let offset = self.pos as u64;
        let slice = self
            .pos
            .checked_add(N)
            .and_then(|end| self.bytes.get(self.pos..end))
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "DNG: gain-table map is truncated")
                    .with_byte_offset(offset)
            })?;
        self.pos += N;
        let mut a = [0u8; N];
        a.copy_from_slice(slice);
        Ok(a)
    }

    fn u16(&mut self) -> Result<u16> {
        self.take::<2>().map(|b| self.order.u16(b))
    }

    fn u32(&mut self) -> Result<u32> {
        self.take::<4>().map(|b| self.order.u32(b))
    }

    fn f32(&mut self) -> Result<f32> {
        self.u32().map(f32::from_bits)
    }

    fn f64(&mut self) -> Result<f64> {
        let b = self.take::<8>()?;
        let bits = match self.order {
            ByteOrder::LittleEndian => u64::from_le_bytes(b),
            ByteOrder::BigEndian => u64::from_be_bytes(b),
        };
        Ok(f64::from_bits(bits))
    }

    /// The 64-byte header shared by both tag versions:
    /// `(points_v, points_h, [spacing_v, spacing_h, origin_v, origin_h], points_n, weights)`.
    #[allow(clippy::type_complexity)]
    fn header(&mut self) -> Result<(u32, u32, [f64; 4], u32, [f32; 5])> {
        let points_v = self.u32()?;
        let points_h = self.u32()?;
        let spacing_v = self.f64()?;
        let spacing_h = self.f64()?;
        let origin_v = self.f64()?;
        let origin_h = self.f64()?;
        let points_n = self.u32()?;
        let mut weights = [0.0f32; 5];
        for w in &mut weights {
            *w = self.f32()?;
        }
        Ok((
            points_v,
            points_h,
            [spacing_v, spacing_h, origin_v, origin_h],
            points_n,
            weights,
        ))
    }
}

/// The serialising counterpart of [`Reader`].
struct Writer {
    out: Vec<u8>,
    order: ByteOrder,
}

impl Writer {
    fn u16(&mut self, x: u16) {
        self.out.extend_from_slice(&self.order.pack_u16(x));
    }

    fn u32(&mut self, x: u32) {
        self.out.extend_from_slice(&self.order.pack_u32(x));
    }

    fn f32(&mut self, x: f32) {
        self.u32(x.to_bits());
    }

    fn f64(&mut self, x: f64) {
        let bits = x.to_bits();
        match self.order {
            ByteOrder::LittleEndian => self.out.extend_from_slice(&bits.to_le_bytes()),
            ByteOrder::BigEndian => self.out.extend_from_slice(&bits.to_be_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_map(gains: GainValues) -> ProfileGainTableMap {
        ProfileGainTableMap {
            points_v: 1,
            points_h: 2,
            spacing_v: 0.5,
            spacing_h: 1.0,
            origin_v: 0.0,
            origin_h: -0.25,
            points_n: 2,
            input_weights: [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0, 0.0, 0.0],
            gamma: 1.0,
            gain_min: 0.0,
            gain_max: 0.0,
            gains,
        }
    }

    /// Hand-computed v1 byte golden (little-endian): a symmetric parse∘serialise identity alone
    /// could not see a transposed field order, so the exact header bytes are written out.
    #[test]
    fn v1_layout_matches_the_spec_bytes() {
        let map = sample_map(GainValues::F32(vec![1.0, 1.5, 2.0, 0.5]));
        let bytes = map.to_bytes_v1(ByteOrder::LittleEndian).expect("serialise");
        assert_eq!(bytes.len(), 64 + 4 * 4);
        let mut expected = Vec::new();
        expected.extend_from_slice(&1u32.to_le_bytes()); // MapPointsV
        expected.extend_from_slice(&2u32.to_le_bytes()); // MapPointsH
        expected.extend_from_slice(&0.5f64.to_le_bytes()); // MapSpacingV
        expected.extend_from_slice(&1.0f64.to_le_bytes()); // MapSpacingH
        expected.extend_from_slice(&0.0f64.to_le_bytes()); // MapOriginV
        expected.extend_from_slice(&(-0.25f64).to_le_bytes()); // MapOriginH
        expected.extend_from_slice(&2u32.to_le_bytes()); // MapPointsN
        for w in [1.0f32 / 3.0, 1.0 / 3.0, 1.0 / 3.0, 0.0, 0.0] {
            expected.extend_from_slice(&w.to_le_bytes());
        }
        for g in [1.0f32, 1.5, 2.0, 0.5] {
            expected.extend_from_slice(&g.to_le_bytes());
        }
        assert_eq!(bytes, expected);

        let parsed = ProfileGainTableMap::parse_v1(&bytes, ByteOrder::LittleEndian).unwrap();
        assert_eq!(parsed, map);
    }

    #[test]
    fn v2_roundtrips_every_data_type_in_both_orders() {
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            for gains in [
                GainValues::U8(vec![0, 128, 255, 64]),
                GainValues::U16(vec![0, 30000, 65535, 42]),
                GainValues::F16(vec![0x3C00, 0x4000, 0x3800, 0]),
                GainValues::F32(vec![1.0, 0.5, 2.0, 1.25]),
            ] {
                let mut map = sample_map(gains);
                map.gamma = 2.2;
                map.gain_min = 0.7;
                map.gain_max = 2.3;
                let bytes = map.to_bytes_v2(order).expect("serialise");
                let parsed = ProfileGainTableMap::parse_v2(&bytes, order).expect("parse");
                assert_eq!(parsed, map, "{order:?}");
                // Byte-exact re-serialisation.
                assert_eq!(parsed.to_bytes_v2(order).unwrap(), bytes);
            }
        }
    }

    /// The v2 header golden: DataType/Gamma/GainMin/GainMax sit at offsets 64..80.
    #[test]
    fn v2_extension_fields_sit_after_the_shared_header() {
        let mut map = sample_map(GainValues::U8(vec![10, 20, 30, 40]));
        map.gamma = 0.5;
        map.gain_min = 0.7;
        map.gain_max = 2.3;
        let bytes = map.to_bytes_v2(ByteOrder::LittleEndian).unwrap();
        assert_eq!(bytes.len(), 80 + 4);
        assert_eq!(&bytes[64..68], &0u32.to_le_bytes()); // DataType 0 = U8
        assert_eq!(&bytes[68..72], &0.5f32.to_le_bytes()); // Gamma
        assert_eq!(&bytes[72..76], &0.7f32.to_le_bytes()); // GainMin
        assert_eq!(&bytes[76..80], &2.3f32.to_le_bytes()); // GainMax
        assert_eq!(&bytes[80..], &[10, 20, 30, 40]);
    }

    #[test]
    fn gain_at_decodes_integer_codes_through_the_gain_range() {
        let mut map = sample_map(GainValues::U8(vec![0, 255, 128, 51]));
        map.gain_min = 0.7;
        map.gain_max = 2.3;
        assert_eq!(map.gain_at(0, 0, 0), Some(0.7));
        assert_eq!(map.gain_at(0, 0, 1), Some(2.3));
        let mid = map.gain_at(0, 1, 0).unwrap();
        assert!((mid - (0.7 + (128.0 / 255.0) * 1.6)).abs() < 1e-6);
        // Out of range is a miss, not a panic.
        assert_eq!(map.gain_at(1, 0, 0), None);
        assert_eq!(map.gain_at(0, 2, 0), None);
        assert_eq!(map.gain_at(0, 0, 2), None);
        // Float storage returns the value directly.
        let f = sample_map(GainValues::F32(vec![1.0, 1.5, 2.0, 0.5]));
        assert_eq!(f.gain_at(0, 1, 1), Some(0.5));
    }

    #[test]
    fn gain_values_len_and_emptiness() {
        assert!(GainValues::F32(vec![]).is_empty());
        assert!(!GainValues::U8(vec![1]).is_empty());
        assert_eq!(GainValues::U16(vec![1, 2, 3]).len(), 3);
        assert_eq!(GainValues::F16(vec![]).len(), 0);
    }

    /// `0.0` is a valid (non-negative) gain — only negatives and non-finites reject.
    #[test]
    fn parse_v1_accepts_a_zero_gain() {
        let map = sample_map(GainValues::F32(vec![0.0, 1.0, 2.0, 3.0]));
        let bytes = map.to_bytes_v1(ByteOrder::LittleEndian).unwrap();
        let parsed = ProfileGainTableMap::parse_v1(&bytes, ByteOrder::LittleEndian).unwrap();
        assert_eq!(parsed.gains, GainValues::F32(vec![0.0, 1.0, 2.0, 3.0]));
    }

    /// Every axis of the zero-dimension check rejects on its own — even with a matching (empty)
    /// gain vector, where a weakened check would happily serialise an 80-byte husk.
    #[test]
    fn zero_dimension_maps_do_not_serialise() {
        for (v, h, n) in [(0u32, 1u32, 1u32), (1, 0, 1), (1, 1, 0)] {
            let mut map = sample_map(GainValues::F32(vec![]));
            map.points_v = v;
            map.points_h = h;
            map.points_n = n;
            assert!(
                map.to_bytes_v1(ByteOrder::LittleEndian).is_err(),
                "{v},{h},{n}"
            );
            assert!(
                map.to_bytes_v2(ByteOrder::LittleEndian).is_err(),
                "{v},{h},{n}"
            );
        }
    }

    /// The 16-bit integer branch of `gain_at` (the 8-bit branch is covered separately): exact
    /// range endpoints plus interior codes pin the `min + (code / 65535) * (max - min)` formula
    /// against every operator mutation.
    #[test]
    fn gain_at_u16_codes_decode_through_the_range_exactly() {
        let mut map = sample_map(GainValues::U16(vec![0, 65535, 13107, 26214]));
        map.gain_min = 0.7;
        map.gain_max = 2.3;
        assert_eq!(map.gain_at(0, 0, 0), Some(0.7));
        assert_eq!(map.gain_at(0, 0, 1), Some(2.3));
        // Codes 13107 and 26214 are exactly 0.2 and 0.4 of the range.
        let g = map.gain_at(0, 1, 0).unwrap();
        assert!((g - (0.7 + 0.2 * 1.6)).abs() < 1e-4, "{g}");
        let g = map.gain_at(0, 1, 1).unwrap();
        assert!((g - (0.7 + 0.4 * 1.6)).abs() < 1e-4, "{g}");
    }

    /// A grid with `points_v > 1` pins the `(v * points_h + h) * points_n + n` indexing, and an
    /// out-of-range `h` whose flat index still lands inside the vector must miss on the
    /// *coordinate* check, not the slice lookup.
    #[test]
    fn gain_at_indexes_the_v_major_grid() {
        let mut map = sample_map(GainValues::F32(vec![1.0, 2.0, 3.0, 4.0]));
        map.points_v = 2;
        map.points_h = 2;
        map.points_n = 1;
        assert_eq!(map.gain_at(0, 0, 0), Some(1.0));
        assert_eq!(map.gain_at(0, 1, 0), Some(2.0));
        assert_eq!(map.gain_at(1, 0, 0), Some(3.0));
        assert_eq!(map.gain_at(1, 1, 0), Some(4.0));
        // h = 2 is out of range, though its flat index (2) is not.
        assert_eq!(map.gain_at(0, 2, 0), None);
        // n = 1 is out of range likewise (flat index 1).
        assert_eq!(map.gain_at(0, 0, 1), None);
    }

    #[test]
    fn parse_rejects_malformed_payloads() {
        let map = sample_map(GainValues::F32(vec![1.0, 1.5, 2.0, 0.5]));
        let v1 = map.to_bytes_v1(ByteOrder::LittleEndian).unwrap();
        // Truncated.
        assert!(
            ProfileGainTableMap::parse_v1(&v1[..v1.len() - 1], ByteOrder::LittleEndian).is_err()
        );
        // Wrong-order parse mangles the count check rather than panicking.
        assert!(ProfileGainTableMap::parse_v1(&v1, ByteOrder::BigEndian).is_err());
        // Negative gain.
        let mut bad = v1.clone();
        bad[64..68].copy_from_slice(&(-1.0f32).to_le_bytes());
        assert!(ProfileGainTableMap::parse_v1(&bad, ByteOrder::LittleEndian).is_err());

        let mut map2 = map.clone();
        map2.gain_min = 0.5;
        map2.gain_max = 2.0;
        let v2 = map2.to_bytes_v2(ByteOrder::LittleEndian).unwrap();
        // Unknown DataType.
        let mut bad = v2.clone();
        bad[64..68].copy_from_slice(&9u32.to_le_bytes());
        assert!(ProfileGainTableMap::parse_v2(&bad, ByteOrder::LittleEndian).is_err());
        // Gamma out of range.
        let mut bad = v2.clone();
        bad[68..72].copy_from_slice(&8.0f32.to_le_bytes());
        assert!(ProfileGainTableMap::parse_v2(&bad, ByteOrder::LittleEndian).is_err());
        // Zero dimensions.
        let mut bad = v2;
        bad[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(ProfileGainTableMap::parse_v2(&bad, ByteOrder::LittleEndian).is_err());
    }

    #[test]
    fn v1_serialisation_rejects_v2_only_content() {
        // Integer gains have no v1 form.
        let map = sample_map(GainValues::U8(vec![1, 2, 3, 4]));
        assert!(map.to_bytes_v1(ByteOrder::LittleEndian).is_err());
        // A non-identity gamma cannot be stored in v1.
        let mut map = sample_map(GainValues::F32(vec![1.0; 4]));
        map.gamma = 2.0;
        assert!(map.to_bytes_v1(ByteOrder::LittleEndian).is_err());
        // A count mismatch is rejected in both directions.
        let mut map = sample_map(GainValues::F32(vec![1.0; 3]));
        map.gain_min = 0.0;
        map.gain_max = 1.0;
        assert!(map.to_bytes_v1(ByteOrder::LittleEndian).is_err());
        map.gamma = 1.0;
        assert!(map.to_bytes_v2(ByteOrder::LittleEndian).is_err());
    }
}
