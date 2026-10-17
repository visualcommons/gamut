//! JPEG-1 transform kernels (ITU-T T.81 | ISO/IEC 10918-1, Annex A).
//!
//! The DCT-based JPEG processes transform each 8×8 block of level-shifted samples with the 2-D
//! discrete cosine transform of T.81 §A.3.3:
//! - the forward DCT ([`fdct8x8`], the encoder transform),
//! - the inverse DCT ([`idct8x8`], the reference decoder transform), and
//! - the fast fixed-point inverse DCT ([`idct8x8_int`], with its DC-only shortcut
//!   [`idct8x8_int_dc`]) that the decoder runs: bit-exact with libjpeg-turbo's accurate integer
//!   IDCT and held to the IEEE 1180-1990 accuracy bounds against [`idct8x8`].
//!
//! All operate in place on a raster-order (row-major / natural) `[i32; 64]` block — the sample
//! and coefficient orientation of T.81 Figure A.4. [`fdct8x8`] and [`idct8x8`] are separable
//! `f64` evaluations of the *informative* §A.3.3 equations with a single final rounding, kept as
//! the reference; [`idct8x8_int`] is a 13-bit fixed-point approximation of the same transform.
//! The surrounding pipeline stages that §A.3 layers around the transform — the §A.3.1 level
//! shift, §A.3.4 quantization, §A.3.5 differential DC, and §A.3.6 zig-zag reordering — are the
//! `gamut-jpeg` codec's responsibility, not this kernel module's.
//!
//! Per T.81 §A.3.3 these ideal equations "cannot be represented with perfect accuracy by any real
//! implementation"; conformance to the normative accuracy bounds of T.83 (ISO/IEC 10918-2) is a
//! codec-level concern. T.83 itself is not vendored (`references/jpeg/README.md`), so
//! [`idct8x8_int`]'s accuracy is tested by the IEEE 1180-1990 procedure, the inverse-DCT accuracy
//! specification JPEG decoders are conventionally held to. A fast *forward* kernel is not
//! provided: [`fdct8x8`] remains the encoder transform.

mod dct;

pub use dct::{fdct8x8, idct8x8, idct8x8_int, idct8x8_int_dc};
