//! The decode-throughput corpus (issue #305): deterministic photo-like RGB content, encoded by the
//! libjpeg-turbo oracle into the stream shapes a camera writes.
//!
//! Shared by `benches/decode.rs` and `examples/experiment_305.rs`, so the bench and the experiment
//! can never describe different streams. Both decoders under comparison read the *same* bytes, and
//! the encoder that wrote them is neither of them: libjpeg-turbo, at its standard Annex K tables.
//!
//! The content is generated, not a camera file, because the repository commits no multi-megabyte
//! JPEG and the experiment must be regenerable from the tree alone. It is built to give the entropy
//! decoder a camera-like workload rather than a flattering one: a smooth illumination field (most of
//! a photograph's area), per-pixel sensor-like noise (which is what keeps AC coefficients alive at
//! q90 and makes the Huffman path the hot one), and hard-edged rectangles (high-frequency bursts).

use libjpeg_oracle::{EncodeParams, Subsampling};

/// One corpus stream: a name and the JPEG bytes libjpeg-turbo wrote.
pub struct Stream {
    /// Row label: process and subsampling, e.g. `baseline-420`.
    pub name: &'static str,
    /// The encoded stream.
    pub jpeg: Vec<u8>,
}

/// The stream shapes measured: what cameras write (baseline 4:2:2 and 4:2:0), the chroma-heavy
/// 4:4:4 case the issue's size comparison was made at, and a progressive stream (libjpeg-turbo's
/// standard ten-scan `jpeg_simple_progression` script), all at quality 90 with the standard tables.
pub const SHAPES: [(&str, Subsampling, bool); 4] = [
    ("baseline-420", Subsampling::S420, false),
    ("baseline-422", Subsampling::S422, false),
    ("baseline-444", Subsampling::S444, false),
    ("progressive-420", Subsampling::S420, true),
];

/// Quality every stream is encoded at: the q90 the issue's measurements were taken at.
pub const QUALITY: i32 = 90;

/// Deterministic photo-like interleaved RGB, `width * height * 3` bytes.
///
/// A 32-bit LCG (Numerical Recipes constants) supplies the noise, seeded by a fixed constant, so the
/// samples are a pure function of the dimensions.
#[must_use]
pub fn photo_rgb(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    // Separable smooth field: a few low-frequency waves per axis, tabulated once per axis.
    let wave = |n: usize, f: [f64; 3]| -> Vec<f64> {
        (0..n)
            .map(|i| {
                let t = i as f64 / n as f64 * std::f64::consts::TAU;
                (f[0] * t).sin() * 40.0
                    + (f[1] * t + 1.3).sin() * 25.0
                    + (f[2] * t + 0.7).sin() * 12.0
            })
            .collect()
    };
    let col = wave(w, [1.0, 3.0, 7.0]);
    let row = wave(h, [2.0, 5.0, 11.0]);
    // Hard-edged rectangles on a coarse grid: one in every few cells carries a flat offset.
    let cell = (w.max(h) / 24).max(8);
    let mut state: u32 = 0x3053_0305;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state >> 8
    };
    let cells_x = w.div_ceil(cell);
    let cells_y = h.div_ceil(cell);
    let mut offsets = vec![0i32; cells_x * cells_y];
    for o in &mut offsets {
        let r = next();
        if r % 5 == 0 {
            *o = (r % 120) as i32 - 60;
        }
    }
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let base = 120.0 + col[x] + row[y];
            let edge = offsets[(y / cell) * cells_x + x / cell];
            for (c, tint) in [(0usize, 18.0), (1, 0.0), (2, -22.0)] {
                let noise = (next() % 17) as i32 - 8;
                let tilt = tint * (x as f64 / w as f64 - y as f64 / h as f64);
                let v = (base + tilt) as i32 + edge + noise + c as i32;
                out.push(v.clamp(0, 255) as u8);
            }
        }
    }
    out
}

/// Encodes the photo-like content at `width x height` into every [`SHAPES`] stream.
///
/// # Panics
///
/// If libjpeg-turbo rejects the encode, which only a broken oracle build does.
#[must_use]
pub fn streams(width: u32, height: u32) -> Vec<Stream> {
    let rgb = photo_rgb(width, height);
    SHAPES
        .iter()
        .map(|&(name, subsampling, progressive)| {
            let params = EncodeParams {
                quality: QUALITY,
                subsampling,
                progressive,
                ..EncodeParams::default()
            };
            let jpeg = libjpeg_oracle::encode(&rgb, width, height, &params)
                .unwrap_or_else(|e| panic!("libjpeg-turbo encode of {name}: {e}"));
            Stream { name, jpeg }
        })
        .collect()
}

/// `(max, mean)` absolute per-sample difference between two equal-length sample buffers.
///
/// # Panics
///
/// If the lengths differ: the decoders disagree on the image geometry, which no tolerance covers.
#[must_use]
pub fn diff(a: &[u8], b: &[u8]) -> (u8, f64) {
    assert_eq!(a.len(), b.len(), "decoders disagree on the sample count");
    let mut max = 0u8;
    let mut sum = 0u64;
    for (&x, &y) in a.iter().zip(b) {
        let d = x.abs_diff(y);
        max = max.max(d);
        sum += u64::from(d);
    }
    (max, sum as f64 / a.len().max(1) as f64)
}
