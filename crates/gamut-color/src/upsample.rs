//! Chroma upsampling: reconstructing full-resolution chroma from a 2:1-subsampled plane, the way
//! each format's reference decoder does it by default.
//!
//! No image specification fixes the reconstruction filter. T.81 §A.2 leaves it open, RFC 6386
//! is silent on it, and AV1 still images name a sample position but no filter. So "correct" output
//! here means the output of the reference decoder the format is judged against. Each of those
//! interpolates by default:
//!
//! | Format | Reference decoder default | Kernel here |
//! |--------|---------------------------|-------------|
//! | JPEG | libjpeg-turbo `djpeg` (`do_fancy_upsampling`, `src/jdsample.c`) | [`libjpeg`] |
//! | WebP | libwebp `dwebp` (fancy upsampler, `src/dsp/upsampling.c`) | [`libwebp`] |
//! | AVIF | libavif `avifImageYUVToRGB` built-in bilinear (`src/reformat.c`) | [`libavif`] |
//!
//! All three use the same **geometry**. Chroma samples sit midway between the two luma samples
//! they cover (centred, not co-sited). Each output sample takes ¾ of its nearest chroma sample
//! and ¼ of the next nearest along each subsampled axis, which comes to 9/16, 3/16, 3/16 and 1/16
//! for 4:2:0. A neighbour past the plane's edge is the edge sample repeated. What differs is the
//! **rounding**, and every reference decoder's output depends on its own rounding. So each kernel
//! is a port of its reference with the rounding exact, and none of them is a special case of
//! another:
//!
//! - libjpeg-turbo rounds each 8-bit result with an alternating bias (an ordered dither). It also
//!   falls back to replication for a 2:1 horizontal plane only one or two samples wide.
//! - libwebp approximates the 9/3/3/1 sum in two truncating stages through the diagonals.
//! - libavif sums in floating point and never rounds chroma before its colour conversion. Its
//!   kernel therefore returns the exact sum in sixteenths of a code value, which
//!   [`YcbcrMatrix::to_rgb_sixteenths`](crate::YcbcrMatrix::to_rgb_sixteenths) converts.
//!
//! [`taps`] picks the two chroma rows under an output row for a 2:1 vertical ratio. It is the same
//! function for all three references.
//!
//! The kernels work a row at a time, so a decoder needs only one scratch row per component, not a
//! full-resolution plane. Each kernel takes the output row length `n` from `out`. The input rows
//! must then hold at least `ceil(n / 2)` samples. That is the subsampled width libjpeg-turbo,
//! libwebp and libavif all use, so a component row padded past it (a JPEG block row, say) can be
//! passed as is. The kernels panic on a shorter row, which is a caller bug and not a data error.

/// How a decoder reconstructs full-resolution chroma from a subsampled plane.
///
/// Every gamut decoder that presents subsampled YCbCr as RGB takes one of these. The default,
/// [`Interpolated`](Self::Interpolated), matches the format's reference decoder (see the
/// [module documentation](self)), so a decoded image sits on the reference's own output. Choose
/// [`Nearest`](Self::Nearest) for the cheaper sample-replicating reconstruction.
///
/// The discriminants are permanent and new variants are only ever appended, so the value crosses
/// a C boundary as a plain `u8`.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ChromaUpsampling {
    /// Sample replication: each chroma sample covers every luma sample it was subsampled from.
    /// Exact at 4:4:4, and the cheapest choice otherwise.
    Nearest = 0,
    /// The format's reference interpolating filter: libjpeg-turbo's fancy upsampling for JPEG,
    /// libwebp's fancy upsampler for WebP, libavif's bilinear filter for AVIF.
    #[default]
    Interpolated = 1,
}

/// The chroma rows `(near, far)` that output row `row` interpolates between, under a 2:1 vertical
/// subsampling of a plane `chroma_rows` rows tall.
///
/// `near` is the row `row` was subsampled into, `row / 2`. `far` is the next nearest row: the one
/// above for an even `row` and the one below for an odd one, clamped to the plane. libjpeg-turbo
/// (the main controller's context rows), libwebp (`EmitFancyRGB`) and libavif (the `uAdjRow`
/// choice) all pick these same two rows.
///
/// # Panics
///
/// Panics in debug builds if `row / 2` is not a row of the plane (`row / 2 >= chroma_rows`).
#[must_use]
pub fn taps(row: usize, chroma_rows: usize) -> (usize, usize) {
    let near = row / 2;
    debug_assert!(near < chroma_rows, "output row past the chroma plane");
    let far = if row.is_multiple_of(2) {
        near.saturating_sub(1)
    } else {
        (near + 1).min(chroma_rows - 1)
    };
    (near, far)
}

/// The subsampled width under an output row `n` samples long, and the index of its last sample.
fn chroma_width(n: usize) -> (usize, usize) {
    let width = n.div_ceil(2);
    (width, width.saturating_sub(1))
}

/// Sample replication along one 2:1 axis: output sample `x` takes chroma sample `x / 2`.
pub(crate) fn replicate(src: &[u8], out: &mut [u8]) {
    for (x, o) in out.iter_mut().enumerate() {
        *o = src[x / 2];
    }
}

/// libjpeg-turbo's fancy upsampling kernels (`src/jdsample.c`, libjpeg-turbo 3.x): the triangle
/// filter `djpeg` applies by default.
///
/// The functions are named after the ratio of the component to the frame's maximum sampling
/// factors, as libjpeg-turbo names them. `h2v1` serves a component at half the horizontal
/// resolution (4:2:2 chroma), `h2v2` half in both directions (4:2:0), and `h1v2` half the vertical
/// (4:4:0). libjpeg-turbo picks the kernel from that ratio, so a component sampled 2 of 4 is
/// upsampled exactly as one sampled 1 of 2. libjpeg-turbo replicates every other integer ratio and
/// refuses a non-integer one.
///
/// Results round with libjpeg-turbo's alternating bias: 0.5 rounds up at one output position and
/// down at the next. A `+ 1`/`+ 2` pair does it at quarter scale and `+ 8`/`+ 7` at sixteenth
/// scale. This is what makes the output byte-identical to `djpeg`'s, where a plain
/// round-half-up would differ.
pub mod libjpeg {
    use super::{chroma_width, replicate};

    /// Upsamples one row of a component at half the horizontal resolution (`h2v1_fancy_upsample`)
    /// into `out`, one output row long.
    ///
    /// Output sample `2c` is `(3·s[c] + s[c-1] + 1) >> 2` and `2c + 1` is
    /// `(3·s[c] + s[c+1] + 2) >> 2`, with the edge sample standing in for a missing neighbour.
    /// Like libjpeg-turbo, a component only one or two samples wide (`ceil(n / 2) <= 2`) is
    /// replicated instead (`h2v1_upsample`).
    ///
    /// # Panics
    ///
    /// Panics if `src` holds fewer than `ceil(out.len() / 2)` samples.
    pub fn h2v1(src: &[u8], out: &mut [u8]) {
        let (width, last) = chroma_width(out.len());
        if width <= 2 {
            replicate(src, out);
            return;
        }
        for (x, o) in out.iter_mut().enumerate() {
            let c = x / 2;
            let this = 3 * u16::from(src[c]);
            *o = if x.is_multiple_of(2) {
                (this + u16::from(src[c.saturating_sub(1)]) + 1) >> 2
            } else {
                (this + u16::from(src[(c + 1).min(last)]) + 2) >> 2
            } as u8;
        }
    }

    /// Upsamples a component at half the resolution in both directions (`h2v2_fancy_upsample`)
    /// into `out`, one output row long. `near` and `far` are the component rows [`taps`]
    /// returns for that output row.
    ///
    /// Each column first sums vertically, `t[c] = 3·near[c] + far[c]`. Then output sample `2c`
    /// is `(3·t[c] + t[c-1] + 8) >> 4` and `2c + 1` is `(3·t[c] + t[c+1] + 7) >> 4`, with the edge
    /// column standing in for a missing neighbour. Like libjpeg-turbo, a component only one or two
    /// samples wide is replicated instead (`h2v2_upsample`). That means `near` is replicated and
    /// `far` is not read.
    ///
    /// # Panics
    ///
    /// Panics if `near` or `far` holds fewer than `ceil(out.len() / 2)` samples.
    ///
    /// [`taps`]: super::taps
    pub fn h2v2(near: &[u8], far: &[u8], out: &mut [u8]) {
        let (width, last) = chroma_width(out.len());
        if width <= 2 {
            replicate(near, out);
            return;
        }
        let sum = |c: usize| 3 * u16::from(near[c]) + u16::from(far[c]);
        for (x, o) in out.iter_mut().enumerate() {
            let c = x / 2;
            let this = 3 * sum(c);
            *o = if x.is_multiple_of(2) {
                (this + sum(c.saturating_sub(1)) + 8) >> 4
            } else {
                (this + sum((c + 1).min(last)) + 7) >> 4
            } as u8;
        }
    }

    /// Upsamples a component at half the vertical resolution (`h1v2_fancy_upsample`) into `out`,
    /// one output row long. `near` and `far` are the component rows [`taps`] returns for that
    /// output row, and `far_below` says which side `far` lies on: `true` for an odd output row.
    ///
    /// Output sample `x` is `(3·near[x] + far[x] + bias) >> 2`, with `bias` 1 when `far` is the
    /// row above and 2 when it is the row below. libjpeg-turbo has no replication fallback for
    /// this ratio.
    ///
    /// # Panics
    ///
    /// Panics if `near` or `far` holds fewer than `out.len()` samples.
    ///
    /// [`taps`]: super::taps
    pub fn h1v2(near: &[u8], far: &[u8], far_below: bool, out: &mut [u8]) {
        let bias = 1 + u16::from(far_below);
        let len = out.len();
        for ((o, &n), &f) in out.iter_mut().zip(&near[..len]).zip(&far[..len]) {
            *o = ((3 * u16::from(n) + u16::from(f) + bias) >> 2) as u8;
        }
    }
}

/// libwebp's fancy upsampler (`UpsampleRgbLinePair` in `src/dsp/upsampling.c`, libwebp 1.5): the
/// 4:2:0 interpolation `dwebp` applies by default.
pub mod libwebp {
    use super::chroma_width;

    /// Upsamples one output row of a 4:2:0 chroma plane into `out`. `near` and `far` are the chroma
    /// rows [`taps`] returns for that output row. The first output row's `far` is `near` itself,
    /// as libwebp's `EmitFancyRGB` mirrors the plane at its top edge.
    ///
    /// The first sample, and the last of an even-length row, interpolate vertically only:
    /// `(3·near + far + 2) >> 2`. Every interior pair between chroma columns `c - 1` and `c`
    /// (call the near samples `a`, `b` and the far ones `c0`, `d`) shares
    /// `avg = a + b + c0 + d + 8`, and then
    ///
    /// - the left sample is `(((avg + 2·(b + c0)) >> 3) + a) >> 1`, and
    /// - the right sample is `(((avg + 2·(a + d)) >> 3) + b) >> 1`.
    ///
    /// That approximates `(9·nearest + 3·adjacent + 3·adjacent + diagonal + 8) >> 4` with two
    /// truncating stages, and the result is libwebp's exactly. libwebp packs U and V into one
    /// `u32`, but no lane can carry into the other, so per-plane evaluation is the same function.
    ///
    /// # Panics
    ///
    /// Panics if `near` or `far` holds fewer than `ceil(out.len() / 2)` samples.
    ///
    /// [`taps`]: super::taps
    pub fn fancy_row(near: &[u8], far: &[u8], out: &mut [u8]) {
        let len = out.len();
        if len == 0 {
            return;
        }
        let (_, last) = chroma_width(len);
        let n = |c: usize| u32::from(near[c]);
        let f = |c: usize| u32::from(far[c]);
        let vertical = |c: usize| ((3 * n(c) + f(c) + 2) >> 2) as u8;
        out[0] = vertical(0);
        for c in 1..=(len - 1) / 2 {
            let avg = n(c - 1) + n(c) + f(c - 1) + f(c) + 8;
            out[2 * c - 1] = ((((avg + 2 * (n(c) + f(c - 1))) >> 3) + n(c - 1)) >> 1) as u8;
            out[2 * c] = ((((avg + 2 * (n(c - 1) + f(c))) >> 3) + n(c)) >> 1) as u8;
        }
        if len.is_multiple_of(2) {
            out[len - 1] = vertical(last);
        }
    }
}

/// libavif's built-in bilinear chroma upsampling (`avifImageYUVAnyToRGBAnySlow` in
/// `src/reformat.c`, libavif 1.4): what `avifImageYUVToRGB` applies by default when libavif is
/// built without libyuv.
pub mod libavif {
    use super::chroma_width;

    /// Upsamples one output row of a 4:2:0 or 4:2:2 chroma plane into `out`, as **sixteenths of a
    /// code value**. For 4:2:0, `near` and `far` are the chroma rows [`taps`] returns for the output
    /// row. For 4:2:2 there is no vertical neighbour, so pass the one chroma row as both.
    ///
    /// Output sample `x` is `9·near[c] + 3·near[a] + 3·far[c] + far[a]`, where `c = x / 2` is its
    /// nearest chroma column and `a` the adjacent one: `c - 1` for an even `x` and `c + 1` for an
    /// odd one, clamped to the row. libavif computes the same weighted sum in floating point and
    /// does not round it before its colour conversion. Returning the exact integer sum keeps that
    /// precision, and [`YcbcrMatrix::to_rgb_sixteenths`] converts it directly. Samples of any
    /// depth up to 16 bits fit, since the largest sum is `16 · 65535`.
    ///
    /// # Panics
    ///
    /// Panics if `near` or `far` holds fewer than `ceil(out.len() / 2)` samples.
    ///
    /// [`taps`]: super::taps
    /// [`YcbcrMatrix::to_rgb_sixteenths`]: crate::YcbcrMatrix::to_rgb_sixteenths
    pub fn bilinear_row(near: &[u16], far: &[u16], out: &mut [u32]) {
        let (_, last) = chroma_width(out.len());
        for (x, o) in out.iter_mut().enumerate() {
            let c = x / 2;
            let a = if x.is_multiple_of(2) {
                c.saturating_sub(1)
            } else {
                (c + 1).min(last)
            };
            *o = 9 * u32::from(near[c])
                + 3 * u32::from(near[a])
                + 3 * u32::from(far[c])
                + u32::from(far[a]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolated_is_the_default_and_discriminants_are_pinned() {
        assert_eq!(ChromaUpsampling::default(), ChromaUpsampling::Interpolated);
        assert_eq!(ChromaUpsampling::Nearest as u8, 0);
        assert_eq!(ChromaUpsampling::Interpolated as u8, 1);
    }

    #[test]
    fn taps_pick_the_row_above_on_even_rows_and_below_on_odd_rows() {
        assert_eq!(taps(0, 3), (0, 0)); // top edge: the row above is the edge row
        assert_eq!(taps(1, 3), (0, 1));
        assert_eq!(taps(2, 3), (1, 0));
        assert_eq!(taps(3, 3), (1, 2));
        assert_eq!(taps(4, 3), (2, 1));
        assert_eq!(taps(5, 3), (2, 2)); // bottom edge: the row below is the edge row
    }

    #[test]
    fn libjpeg_h2v1_matches_jdsample_by_hand() {
        // h2v1_fancy_upsample over [10, 101, 200]: first column 10, (30 + 101 + 2) >> 2 = 33;
        // middle (303 + 10 + 1) >> 2 = 78, (303 + 200 + 2) >> 2 = 126; last (600 + 101 + 1) >> 2 =
        // 175, 200.
        let mut out = [0u8; 6];
        libjpeg::h2v1(&[10, 101, 200], &mut out);
        assert_eq!(out, [10, 33, 78, 126, 175, 200]);
        // An odd output width drops the last pair's second sample, and padding past
        // `ceil(n / 2)` is not read: the last real column still repeats itself.
        let mut odd = [0u8; 5];
        libjpeg::h2v1(&[10, 101, 200, 0], &mut odd);
        assert_eq!(odd, [10, 33, 78, 126, 175]);
    }

    #[test]
    fn libjpeg_h2v1_rounding_bias_alternates() {
        // Around the centre sample, 3·2 + 0 = 6 on both sides: the even output rounds with + 1,
        // (6 + 1) >> 2 = 1, and the odd one with + 2, (6 + 2) >> 2 = 2.
        let mut out = [0u8; 6];
        libjpeg::h2v1(&[0, 2, 0], &mut out);
        assert_eq!(out, [0, 1, 1, 2, 0, 0]);
        // With a centre of 1, 3·1 + 0 = 3 on both sides: the even output's + 1 is what lifts it,
        // (3 + 1) >> 2 = 1 where 3 >> 2 would be 0.
        libjpeg::h2v1(&[0, 1, 0], &mut out);
        assert_eq!(out, [0, 0, 1, 1, 0, 0]);
    }

    #[test]
    fn libjpeg_h2v_replicates_two_sample_components() {
        let mut out = [0u8; 4];
        libjpeg::h2v1(&[0, 100], &mut out);
        assert_eq!(out, [0, 0, 100, 100]);
        libjpeg::h2v2(&[0, 100], &[200, 200], &mut out);
        assert_eq!(out, [0, 0, 100, 100]);
        // Three samples wide (output 5 or 6) already interpolates.
        let mut five = [0u8; 5];
        libjpeg::h2v1(&[0, 100, 100], &mut five);
        assert_eq!(five, [0, 25, 75, 100, 100]);
    }

    #[test]
    fn libjpeg_h2v2_matches_jdsample_by_hand() {
        // Column sums t = 3·near + far = [30, 3·101 + 1 = 304, 600 + 4 = 604].
        // Outputs: (4·30 + 8) >> 4 = 8; (90 + 304 + 7) >> 4 = 25; (912 + 30 + 8) >> 4 = 59;
        // (912 + 604 + 7) >> 4 = 95; (1812 + 304 + 8) >> 4 = 132; (4·604 + 7) >> 4 = 151.
        let mut out = [0u8; 6];
        libjpeg::h2v2(&[10, 101, 200], &[0, 1, 4], &mut out);
        assert_eq!(out, [8, 25, 59, 95, 132, 151]);
    }

    #[test]
    fn libjpeg_h2v2_rounding_bias_alternates() {
        // near = far = [0, 2, 0] gives column sums t = [0, 8, 0]. Around the centre, 3·8 + 0 = 24:
        // the even output rounds with + 8, (24 + 8) >> 4 = 2, and the odd one with + 7,
        // (24 + 7) >> 4 = 1. Beside it, (0 + 8 + 7) >> 4 = 0 but (0 + 8 + 8) >> 4 = 1.
        let mut out = [0u8; 6];
        libjpeg::h2v2(&[0, 2, 0], &[0, 2, 0], &mut out);
        assert_eq!(out, [0, 0, 2, 1, 1, 0]);
    }

    #[test]
    fn libjpeg_h1v2_biases_by_the_far_row_side() {
        // 3·2 + 0 = 6: above (6 + 1) >> 2 = 1, below (6 + 2) >> 2 = 2.
        let mut out = [0u8; 3];
        libjpeg::h1v2(&[2, 100, 255], &[0, 200, 255], false, &mut out);
        assert_eq!(out, [1, 125, 255]);
        libjpeg::h1v2(&[2, 100, 255], &[0, 200, 255, 9], true, &mut out);
        assert_eq!(out, [2, 125, 255]);
    }

    #[test]
    fn libwebp_fancy_row_matches_upsampling_c_by_hand() {
        // near = [10, 100, 200], far = [20, 60, 0], len 6 (even: the last sample is vertical).
        // first: (30 + 20 + 2) >> 2 = 13.
        // pair c=1: avg = 10 + 100 + 20 + 60 + 8 = 198;
        //   left  ((198 + 2·(100 + 20)) >> 3 + 10) >> 1 = (54 + 10) >> 1 = 32
        //   right ((198 + 2·(10 + 60)) >> 3 + 100) >> 1 = (42 + 100) >> 1 = 71
        // pair c=2: avg = 100 + 200 + 60 + 0 + 8 = 368;
        //   left  ((368 + 2·(200 + 60)) >> 3 + 100) >> 1 = (111 + 100) >> 1 = 105
        //   right ((368 + 2·(100 + 0)) >> 3 + 200) >> 1 = (71 + 200) >> 1 = 135
        // last: (600 + 0 + 2) >> 2 = 150.
        let mut out = [0u8; 6];
        libwebp::fancy_row(&[10, 100, 200], &[20, 60, 0], &mut out);
        assert_eq!(out, [13, 32, 71, 105, 135, 150]);
        // An odd length ends on a pair's right sample, with no vertical-only tail.
        let mut odd = [0u8; 5];
        libwebp::fancy_row(&[10, 100, 200], &[20, 60, 0], &mut odd);
        assert_eq!(odd, [13, 32, 71, 105, 135]);
    }

    #[test]
    fn libwebp_fancy_row_handles_one_and_two_sample_rows() {
        let mut one = [0u8; 1];
        libwebp::fancy_row(&[9], &[1], &mut one);
        assert_eq!(one, [7]); // (27 + 1 + 2) >> 2
        let mut two = [0u8; 2];
        libwebp::fancy_row(&[9], &[1], &mut two);
        assert_eq!(two, [7, 7]);
        let mut none: [u8; 0] = [];
        libwebp::fancy_row(&[], &[], &mut none);
    }

    #[test]
    fn libavif_bilinear_row_sums_in_sixteenths() {
        // near = [10, 100, 200], far = [20, 60, 0], len 5.
        // x0: c0, a0: 9·10 + 3·10 + 3·20 + 20 = 200
        // x1: c0, a1: 90 + 300 + 60 + 60 = 510
        // x2: c1, a0: 900 + 30 + 180 + 20 = 1130
        // x3: c1, a2: 900 + 600 + 180 + 0 = 1680
        // x4: c2, a1: 1800 + 300 + 0 + 60 = 2160
        let mut out = [0u32; 5];
        libavif::bilinear_row(&[10, 100, 200], &[20, 60, 0], &mut out);
        assert_eq!(out, [200, 510, 1130, 1680, 2160]);
        // An even length's last sample (odd x at the edge) repeats the edge column, and a row
        // padded past `ceil(n / 2)` is not read.
        let mut even = [0u32; 6];
        libavif::bilinear_row(&[10, 100, 200, 7], &[20, 60, 0, 7], &mut even);
        // x5: c2, a2: 9·200 + 3·200 + 0 + 0 = 2400.
        assert_eq!(even[5], 2400);
        // 16-bit samples do not overflow.
        let mut max = [0u32; 2];
        libavif::bilinear_row(&[u16::MAX], &[u16::MAX], &mut max);
        assert_eq!(max, [16 * 65535, 16 * 65535]);
    }
}
