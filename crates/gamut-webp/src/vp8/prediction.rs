//! VP8 intra prediction (RFC 6386 §11–§12) and key-frame mode coding. Key-frame intra only.
//!
//! Key frames code, per macroblock, a luma 16×16 mode and a chroma 8×8 mode, each tree-coded over the
//! boolean coder with fixed key-frame probabilities (§11.2, §11.4). [`predict_block`] handles the four
//! whole-block modes (DC/V/H/TM); [`subblock_predict`] the ten 4×4 `B_PRED` submodes, whose modes are
//! themselves context-coded against the above/left neighbors via [`KF_BMODE_PROB`] (§11.3). Modes
//! are carried as `usize` indices throughout: the `*_PRED` (whole-block) and `B_*_PRED` (subblock)
//! constants are the same values as the tree-leaf indices the coders use.

use gamut_dsp::math::clip_pixel8;

use super::bool_coder::{Prob, Tree};

/// `DC_PRED` whole-block mode index — luma 16×16 and chroma 8×8 (RFC 6386 §11.2, §12.2). The codec
/// works in these `usize` mode indices throughout.
pub const DC_PRED: usize = 0;
/// `V_PRED` (vertical) whole-block mode index.
pub const V_PRED: usize = 1;
/// `H_PRED` (horizontal) whole-block mode index.
pub const H_PRED: usize = 2;
/// `TM_PRED` (TrueMotion) whole-block mode index.
pub const TM_PRED: usize = 3;
/// `B_PRED` (per-4×4-subblock luma) mode index.
pub const B_PRED: usize = 4;

/// Key-frame luma 16×16 mode tree (RFC 6386 §11.2 / §8.2 `kf_ymode_tree`). Leaf values are the mode
/// indices above (`-4` = `B_PRED`, `0` = `DC_PRED`, `-1` = `V_PRED`, …).
#[rustfmt::skip]
pub const KF_YMODE_TREE: &Tree = &[-4, 2, 4, 6, 0, -1, -2, -3];

/// Key-frame luma 16×16 mode probabilities (RFC 6386 §11.2 `kf_ymode_prob`).
pub const KF_YMODE_PROB: [Prob; 4] = [145, 156, 163, 128];

/// Chroma 8×8 mode tree (RFC 6386 §11.4 / §8.2 `uv_mode_tree`).
#[rustfmt::skip]
pub const KF_UV_MODE_TREE: &Tree = &[0, 2, -1, 4, -2, -3];

/// Key-frame chroma 8×8 mode probabilities (RFC 6386 §11.4 `kf_uv_mode_prob`).
pub const KF_UV_MODE_PROB: [Prob; 3] = [142, 114, 183];

/// Computes the single `DC_PRED` value filling an `n`×`n` block (RFC 6386 §12.2/§12.3): the rounded
/// average of the available reconstructed neighbors. `above` is the `n`-pixel row immediately above
/// the block and `left` the `n`-pixel column immediately to its left, each `None` when off-frame.
/// With neither neighbor (the top-left block) the value is the constant 128; with one, only that
/// neighbor is averaged (the spec's edge exception, *not* the 127/129 out-of-bounds fills).
#[must_use]
pub fn dc_predict(n: usize, above: Option<&[u8]>, left: Option<&[u8]>) -> u8 {
    let sum_of = |pixels: &[u8]| pixels.iter().map(|&p| u32::from(p)).sum::<u32>();
    let round_shift = |sum: u32, shf: u32| ((sum + (1 << (shf - 1))) >> shf) as u8;
    match (above, left) {
        // 2n summands -> shf = log2(2n) (5 for luma, 4 for chroma).
        (Some(a), Some(l)) => round_shift(sum_of(a) + sum_of(l), (2 * n).trailing_zeros()),
        // n summands -> shf = log2(n) (4 for luma, 3 for chroma).
        (Some(a), None) => round_shift(sum_of(a), n.trailing_zeros()),
        (None, Some(l)) => round_shift(sum_of(l), n.trailing_zeros()),
        (None, None) => 128,
    }
}

/// The `n` neighbor pixels, substituting `fill` for an off-frame edge.
fn edge(pixels: Option<&[u8]>, fill: u8) -> [u8; 16] {
    let mut e = [fill; 16];
    if let Some(p) = pixels {
        e[..p.len()].copy_from_slice(p);
    }
    e
}

/// Fills `out[..n*n]` (row-major) with the whole-block prediction for `mode` — `DC_PRED`, `V_PRED`,
/// `H_PRED`, or `TM_PRED` (RFC 6386 §12.2/§12.3). `above`/`left` are the `n` reconstructed neighbor
/// pixels (`None` off-frame: V/H/TM substitute the 127/129 out-of-bounds values, while DC uses its
/// averaging edge exception); `corner` is the above-left pixel TrueMotion propagates from.
pub fn predict_block(
    mode: usize,
    n: usize,
    above: Option<&[u8]>,
    left: Option<&[u8]>,
    corner: u8,
    out: &mut [u8],
) {
    match mode {
        V_PRED => {
            let a = edge(above, 127);
            for r in 0..n {
                out[r * n..r * n + n].copy_from_slice(&a[..n]);
            }
        }
        H_PRED => {
            let l = edge(left, 129);
            for r in 0..n {
                out[r * n..r * n + n].fill(l[r]);
            }
        }
        TM_PRED => {
            let a = edge(above, 127);
            let l = edge(left, 129);
            let p = i32::from(corner);
            for r in 0..n {
                for c in 0..n {
                    out[r * n + c] = clip_pixel8(i32::from(l[r]) + i32::from(a[c]) - p);
                }
            }
        }
        _ => out[..n * n].fill(dc_predict(n, above, left)),
    }
}

/// `B_DC_PRED` 4×4 subblock mode (RFC 6386 §11.2 `intra_bmode`); the leaf values of [`BMODE_TREE`].
pub const B_DC_PRED: usize = 0;
/// `B_TM_PRED` (TrueMotion) 4×4 subblock mode.
pub const B_TM_PRED: usize = 1;
/// `B_VE_PRED` (vertical, smoothed) 4×4 subblock mode.
pub const B_VE_PRED: usize = 2;
/// `B_HE_PRED` (horizontal, smoothed) 4×4 subblock mode.
pub const B_HE_PRED: usize = 3;
/// `B_LD_PRED` (left-down diagonal) 4×4 subblock mode.
pub const B_LD_PRED: usize = 4;
/// `B_RD_PRED` (right-down diagonal) 4×4 subblock mode.
pub const B_RD_PRED: usize = 5;
/// `B_VR_PRED` (vertical-right diagonal) 4×4 subblock mode.
pub const B_VR_PRED: usize = 6;
/// `B_VL_PRED` (vertical-left diagonal) 4×4 subblock mode.
pub const B_VL_PRED: usize = 7;
/// `B_HD_PRED` (horizontal-down diagonal) 4×4 subblock mode.
pub const B_HD_PRED: usize = 8;
/// `B_HU_PRED` (horizontal-up diagonal) 4×4 subblock mode.
pub const B_HU_PRED: usize = 9;
/// Number of 4×4 luma subblock prediction modes.
pub const NUM_BMODES: usize = 10;

/// Subblock-mode coding tree (RFC 6386 §11.2 `bmode_tree`). Leaf `-v` (and `0`) is mode `v`.
#[rustfmt::skip]
pub const BMODE_TREE: &Tree = &[
     0,  2,   -1,  4,   -2,  6,    8, 12,
    -3, 10,   -5, -6,   -4, 14,   -7, 16,
    -8, -9,
];

/// Maps a whole-block luma mode to the subblock mode a non-`B_PRED` macroblock contributes to its
/// neighbors' subblock-mode context (RFC 6386 §11.3 caveat 4).
#[must_use]
pub fn bmode_for_luma(luma_mode: usize) -> usize {
    match luma_mode {
        V_PRED => B_VE_PRED,
        H_PRED => B_HE_PRED,
        TM_PRED => B_TM_PRED,
        _ => B_DC_PRED,
    }
}

/// Weighted 3-tap average centered on `y` (RFC 6386 §12.3 `avg3`).
fn avg3(x: i32, y: i32, z: i32) -> u8 {
    ((x + 2 * y + z + 2) >> 2) as u8
}

/// Simple 2-tap average (RFC 6386 §12.3 `avg2`).
fn avg2(x: i32, y: i32) -> u8 {
    ((x + y + 1) >> 1) as u8
}

/// Predicts one 4×4 luma subblock under `B_PRED` submode `mode` (RFC 6386 §12.3). `above` holds the
/// eight pixels `A[0..8]` of the row above (four directly above, four above-right), `left` the four
/// `L[0..4]` to the left, and `corner` the above-left pixel `A[-1] == L[-1]`. Returns the row-major
/// 4×4 prediction; the diagonal modes synthesize predictors from the `E` edge `[L3 L2 L1 L0 P A0..A3]`.
#[must_use]
pub fn subblock_predict(mode: usize, above: &[u8; 8], left: &[u8; 4], corner: u8) -> [u8; 16] {
    // ax[0]=P, ax[1..9]=A[0..8]; lx[0]=P, lx[1..5]=L[0..4]; e = bottom-left..top-right edge.
    let mut ax = [i32::from(corner); 9];
    for i in 0..8 {
        ax[i + 1] = i32::from(above[i]);
    }
    let mut lx = [i32::from(corner); 5];
    for i in 0..4 {
        lx[i + 1] = i32::from(left[i]);
    }
    let e = [
        lx[4],
        lx[3],
        lx[2],
        lx[1],
        i32::from(corner),
        ax[1],
        ax[2],
        ax[3],
        ax[4],
    ];
    let a3 = |j: usize| avg3(ax[j], ax[j + 1], ax[j + 2]); // avg3p(A + j)
    let a2 = |j: usize| avg2(ax[j + 1], ax[j + 2]); // avg2p(A + j)
    let l3 = |r: usize| avg3(lx[r], lx[r + 1], lx[r + 2]); // avg3p(L + r)
    let e3 = |i: usize| avg3(e[i - 1], e[i], e[i + 1]); // avg3p(E + i)
    let e2 = |i: usize| avg2(e[i], e[i + 1]); // avg2p(E + i)

    let mut b = [0u8; 16];
    let mut set = |r: usize, c: usize, v: u8| b[r * 4 + c] = v;
    match mode {
        B_TM_PRED => {
            for r in 0..4 {
                for c in 0..4 {
                    set(r, c, clip_pixel8(lx[r + 1] + ax[c + 1] - i32::from(corner)));
                }
            }
        }
        B_VE_PRED => {
            for c in 0..4 {
                let v = a3(c);
                for r in 0..4 {
                    set(r, c, v);
                }
            }
        }
        B_HE_PRED => {
            let rows = [l3(0), l3(1), l3(2), avg3(lx[3], lx[4], lx[4])];
            for (r, &row) in rows.iter().enumerate() {
                for c in 0..4 {
                    set(r, c, row);
                }
            }
        }
        B_LD_PRED => {
            set(0, 0, a3(1));
            set(0, 1, a3(2));
            set(1, 0, a3(2));
            set(0, 2, a3(3));
            set(1, 1, a3(3));
            set(2, 0, a3(3));
            set(0, 3, a3(4));
            set(1, 2, a3(4));
            set(2, 1, a3(4));
            set(3, 0, a3(4));
            set(1, 3, a3(5));
            set(2, 2, a3(5));
            set(3, 1, a3(5));
            set(2, 3, a3(6));
            set(3, 2, a3(6));
            set(3, 3, avg3(ax[7], ax[8], ax[8]));
        }
        B_RD_PRED => {
            set(3, 0, e3(1));
            set(3, 1, e3(2));
            set(2, 0, e3(2));
            set(3, 2, e3(3));
            set(2, 1, e3(3));
            set(1, 0, e3(3));
            set(3, 3, e3(4));
            set(2, 2, e3(4));
            set(1, 1, e3(4));
            set(0, 0, e3(4));
            set(2, 3, e3(5));
            set(1, 2, e3(5));
            set(0, 1, e3(5));
            set(1, 3, e3(6));
            set(0, 2, e3(6));
            set(0, 3, e3(7));
        }
        B_VR_PRED => {
            set(3, 0, e3(2));
            set(2, 0, e3(3));
            set(3, 1, e3(4));
            set(1, 0, e3(4));
            set(2, 1, e2(4));
            set(0, 0, e2(4));
            set(3, 2, e3(5));
            set(1, 1, e3(5));
            set(2, 2, e2(5));
            set(0, 1, e2(5));
            set(3, 3, e3(6));
            set(1, 2, e3(6));
            set(2, 3, e2(6));
            set(0, 2, e2(6));
            set(1, 3, e3(7));
            set(0, 3, e2(7));
        }
        B_VL_PRED => {
            set(0, 0, a2(0));
            set(1, 0, a3(1));
            set(2, 0, a2(1));
            set(0, 1, a2(1));
            set(1, 1, a3(2));
            set(3, 0, a3(2));
            set(2, 1, a2(2));
            set(0, 2, a2(2));
            set(3, 1, a3(3));
            set(1, 2, a3(3));
            set(2, 2, a2(3));
            set(0, 3, a2(3));
            set(3, 2, a3(4));
            set(1, 3, a3(4));
            set(2, 3, a3(5));
            set(3, 3, a3(6));
        }
        B_HD_PRED => {
            set(3, 0, e2(0));
            set(3, 1, e3(1));
            set(2, 0, e2(1));
            set(3, 2, e2(1));
            set(2, 1, e3(2));
            set(3, 3, e3(2));
            set(2, 2, e2(2));
            set(1, 0, e2(2));
            set(2, 3, e3(3));
            set(1, 1, e3(3));
            set(1, 2, e2(3));
            set(0, 0, e2(3));
            set(1, 3, e3(4));
            set(0, 1, e3(4));
            set(0, 2, e3(5));
            set(0, 3, e3(6));
        }
        B_HU_PRED => {
            set(0, 0, avg2(lx[1], lx[2]));
            set(0, 1, l3(1));
            set(0, 2, avg2(lx[2], lx[3]));
            set(1, 0, avg2(lx[2], lx[3]));
            set(0, 3, l3(2));
            set(1, 1, l3(2));
            set(1, 2, avg2(lx[3], lx[4]));
            set(2, 0, avg2(lx[3], lx[4]));
            set(1, 3, avg3(lx[3], lx[4], lx[4]));
            set(2, 1, avg3(lx[3], lx[4], lx[4]));
            let l_last = lx[4] as u8;
            for (r, c) in [(2, 2), (2, 3), (3, 0), (3, 1), (3, 2), (3, 3)] {
                set(r, c, l_last);
            }
        }
        _ => {
            // B_DC_PRED: average of the four above + four left, no edge exception.
            let v =
                ((ax[1] + ax[2] + ax[3] + ax[4] + lx[1] + lx[2] + lx[3] + lx[4] + 4) >> 3) as u8;
            b = [v; 16];
        }
    }
    b
}

/// Key-frame subblock-mode probabilities `[above_mode][left_mode][tree_node]` (RFC 6386 §11.3/§11.5).
#[rustfmt::skip]
pub const KF_BMODE_PROB: [[[Prob; 9]; NUM_BMODES]; NUM_BMODES] = [
    [
        [231, 120,  48,  89, 115, 113, 120, 152, 112],
        [152, 179,  64, 126, 170, 118,  46,  70,  95],
        [175,  69, 143,  80,  85,  82,  72, 155, 103],
        [ 56,  58,  10, 171, 218, 189,  17,  13, 152],
        [144,  71,  10,  38, 171, 213, 144,  34,  26],
        [114,  26,  17, 163,  44, 195,  21,  10, 173],
        [121,  24,  80, 195,  26,  62,  44,  64,  85],
        [170,  46,  55,  19, 136, 160,  33, 206,  71],
        [ 63,  20,   8, 114, 114, 208,  12,   9, 226],
        [ 81,  40,  11,  96, 182,  84,  29,  16,  36],
    ],
    [
        [134, 183,  89, 137,  98, 101, 106, 165, 148],
        [ 72, 187, 100, 130, 157, 111,  32,  75,  80],
        [ 66, 102, 167,  99,  74,  62,  40, 234, 128],
        [ 41,  53,   9, 178, 241, 141,  26,   8, 107],
        [104,  79,  12,  27, 217, 255,  87,  17,   7],
        [ 74,  43,  26, 146,  73, 166,  49,  23, 157],
        [ 65,  38, 105, 160,  51,  52,  31, 115, 128],
        [ 87,  68,  71,  44, 114,  51,  15, 186,  23],
        [ 47,  41,  14, 110, 182, 183,  21,  17, 194],
        [ 66,  45,  25, 102, 197, 189,  23,  18,  22],
    ],
    [
        [ 88,  88, 147, 150,  42,  46,  45, 196, 205],
        [ 43,  97, 183, 117,  85,  38,  35, 179,  61],
        [ 39,  53, 200,  87,  26,  21,  43, 232, 171],
        [ 56,  34,  51, 104, 114, 102,  29,  93,  77],
        [107,  54,  32,  26,  51,   1,  81,  43,  31],
        [ 39,  28,  85, 171,  58, 165,  90,  98,  64],
        [ 34,  22, 116, 206,  23,  34,  43, 166,  73],
        [ 68,  25, 106,  22,  64, 171,  36, 225, 114],
        [ 34,  19,  21, 102, 132, 188,  16,  76, 124],
        [ 62,  18,  78,  95,  85,  57,  50,  48,  51],
    ],
    [
        [193, 101,  35, 159, 215, 111,  89,  46, 111],
        [ 60, 148,  31, 172, 219, 228,  21,  18, 111],
        [112, 113,  77,  85, 179, 255,  38, 120, 114],
        [ 40,  42,   1, 196, 245, 209,  10,  25, 109],
        [100,  80,   8,  43, 154,   1,  51,  26,  71],
        [ 88,  43,  29, 140, 166, 213,  37,  43, 154],
        [ 61,  63,  30, 155,  67,  45,  68,   1, 209],
        [142,  78,  78,  16, 255, 128,  34, 197, 171],
        [ 41,  40,   5, 102, 211, 183,   4,   1, 221],
        [ 51,  50,  17, 168, 209, 192,  23,  25,  82],
    ],
    [
        [125,  98,  42,  88, 104,  85, 117, 175,  82],
        [ 95,  84,  53,  89, 128, 100, 113, 101,  45],
        [ 75,  79, 123,  47,  51, 128,  81, 171,   1],
        [ 57,  17,   5,  71, 102,  57,  53,  41,  49],
        [115,  21,   2,  10, 102, 255, 166,  23,   6],
        [ 38,  33,  13, 121,  57,  73,  26,   1,  85],
        [ 41,  10,  67, 138,  77, 110,  90,  47, 114],
        [101,  29,  16,  10,  85, 128, 101, 196,  26],
        [ 57,  18,  10, 102, 102, 213,  34,  20,  43],
        [117,  20,  15,  36, 163, 128,  68,   1,  26],
    ],
    [
        [138,  31,  36, 171,  27, 166,  38,  44, 229],
        [ 67,  87,  58, 169,  82, 115,  26,  59, 179],
        [ 63,  59,  90, 180,  59, 166,  93,  73, 154],
        [ 40,  40,  21, 116, 143, 209,  34,  39, 175],
        [ 57,  46,  22,  24, 128,   1,  54,  17,  37],
        [ 47,  15,  16, 183,  34, 223,  49,  45, 183],
        [ 46,  17,  33, 183,   6,  98,  15,  32, 183],
        [ 65,  32,  73, 115,  28, 128,  23, 128, 205],
        [ 40,   3,   9, 115,  51, 192,  18,   6, 223],
        [ 87,  37,   9, 115,  59,  77,  64,  21,  47],
    ],
    [
        [104,  55,  44, 218,   9,  54,  53, 130, 226],
        [ 64,  90,  70, 205,  40,  41,  23,  26,  57],
        [ 54,  57, 112, 184,   5,  41,  38, 166, 213],
        [ 30,  34,  26, 133, 152, 116,  10,  32, 134],
        [ 75,  32,  12,  51, 192, 255, 160,  43,  51],
        [ 39,  19,  53, 221,  26, 114,  32,  73, 255],
        [ 31,   9,  65, 234,   2,  15,   1, 118,  73],
        [ 88,  31,  35,  67, 102,  85,  55, 186,  85],
        [ 56,  21,  23, 111,  59, 205,  45,  37, 192],
        [ 55,  38,  70, 124,  73, 102,   1,  34,  98],
    ],
    [
        [102,  61,  71,  37,  34,  53,  31, 243, 192],
        [ 69,  60,  71,  38,  73, 119,  28, 222,  37],
        [ 68,  45, 128,  34,   1,  47,  11, 245, 171],
        [ 62,  17,  19,  70, 146,  85,  55,  62,  70],
        [ 75,  15,   9,   9,  64, 255, 184, 119,  16],
        [ 37,  43,  37, 154, 100, 163,  85, 160,   1],
        [ 63,   9,  92, 136,  28,  64,  32, 201,  85],
        [ 86,   6,  28,   5,  64, 255,  25, 248,   1],
        [ 56,   8,  17, 132, 137, 255,  55, 116, 128],
        [ 58,  15,  20,  82, 135,  57,  26, 121,  40],
    ],
    [
        [164,  50,  31, 137, 154, 133,  25,  35, 218],
        [ 51, 103,  44, 131, 131, 123,  31,   6, 158],
        [ 86,  40,  64, 135, 148, 224,  45, 183, 128],
        [ 22,  26,  17, 131, 240, 154,  14,   1, 209],
        [ 83,  12,  13,  54, 192, 255,  68,  47,  28],
        [ 45,  16,  21,  91,  64, 222,   7,   1, 197],
        [ 56,  21,  39, 155,  60, 138,  23, 102, 213],
        [ 85,  26,  85,  85, 128, 128,  32, 146, 171],
        [ 18,  11,   7,  63, 144, 171,   4,   4, 246],
        [ 35,  27,  10, 146, 174, 171,  12,  26, 128],
    ],
    [
        [190,  80,  35,  99, 180,  80, 126,  54,  45],
        [ 85, 126,  47,  87, 176,  51,  41,  20,  32],
        [101,  75, 128, 139, 118, 146, 116, 128,  85],
        [ 56,  41,  15, 176, 236,  85,  37,   9,  62],
        [146,  36,  19,  30, 171, 255,  97,  27,  20],
        [ 71,  30,  17, 119, 118, 255,  17,  18, 138],
        [101,  38,  60, 138,  55,  70,  43,  26, 142],
        [138,  45,  61,  62, 219,   1,  81, 188,  64],
        [ 32,  41,  20, 117, 151, 142,  20,  21, 163],
        [112,  19,  12,  61, 195, 128,  48,   4,  24],
    ],
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subblock_predict_known_vectors() {
        // Absolute per-mode output for fixed neighbors pins every B_PRED submode's averaging
        // arithmetic — including B_DC_PRED, which the encoder's content-driven mode selection rarely
        // chooses (so the differential oracle never exercises it). above = A[0..8], left = L[0..4],
        // corner P = 60.
        let above = [10u8, 40, 70, 100, 130, 160, 190, 220];
        let left = [20u8, 50, 80, 110];
        let expected: [[u8; 16]; NUM_BMODES] = [
            [
                60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60,
            ],
            [
                0, 0, 30, 60, 0, 30, 60, 90, 30, 60, 90, 120, 60, 90, 120, 150,
            ],
            [
                30, 40, 70, 100, 30, 40, 70, 100, 30, 40, 70, 100, 30, 40, 70, 100,
            ],
            [
                38, 38, 38, 38, 50, 50, 50, 50, 80, 80, 80, 80, 103, 103, 103, 103,
            ],
            [
                40, 70, 100, 130, 70, 100, 130, 160, 100, 130, 160, 190, 130, 160, 190, 213,
            ],
            [
                38, 30, 40, 70, 38, 38, 30, 40, 50, 38, 38, 30, 80, 50, 38, 38,
            ],
            [
                35, 25, 55, 85, 38, 30, 40, 70, 38, 35, 25, 55, 50, 38, 30, 40,
            ],
            [
                25, 55, 85, 115, 40, 70, 100, 130, 55, 85, 115, 160, 70, 100, 130, 190,
            ],
            [
                40, 38, 30, 40, 35, 38, 40, 38, 65, 50, 35, 38, 95, 80, 65, 50,
            ],
            [
                35, 50, 65, 80, 65, 80, 95, 103, 95, 103, 110, 110, 110, 110, 110, 110,
            ],
        ];
        for (mode, want) in expected.iter().enumerate() {
            assert_eq!(
                subblock_predict(mode, &above, &left, 60),
                *want,
                "subblock_predict mode {mode} mismatch"
            );
        }
    }

    #[test]
    fn dc_top_left_is_128() {
        assert_eq!(dc_predict(16, None, None), 128);
        assert_eq!(dc_predict(8, None, None), 128);
    }

    #[test]
    fn dc_single_edge_averages_that_edge() {
        let above = [100u8; 16];
        assert_eq!(dc_predict(16, Some(&above), None), 100);
        let left = [10u8, 10, 10, 10, 20, 20, 20, 20];
        let expected = ((10 * 4 + 20 * 4 + 4) >> 3) as u8;
        assert_eq!(dc_predict(8, None, Some(&left)), expected);
    }

    #[test]
    fn dc_both_edges_average_all() {
        let a = [64u8; 16];
        let l = [64u8; 16];
        assert_eq!(dc_predict(16, Some(&a), Some(&l)), 64);
        let a2 = [200u8; 16];
        let l2 = [0u8; 16];
        assert_eq!(
            dc_predict(16, Some(&a2), Some(&l2)),
            ((200 * 16 + 16) >> 5) as u8
        );
    }

    #[test]
    fn mode_trees_are_well_formed() {
        assert_eq!(KF_YMODE_TREE.len(), 8);
        assert_eq!(KF_UV_MODE_TREE.len(), 6);
    }

    #[test]
    fn vertical_prediction_copies_the_above_row() {
        let above: Vec<u8> = (0..16).map(|i| (i * 10) as u8).collect();
        let mut out = [0u8; 256];
        predict_block(V_PRED, 16, Some(&above), Some(&[50u8; 16]), 200, &mut out);
        for r in 0..16 {
            assert_eq!(&out[r * 16..r * 16 + 16], &above[..]);
        }
    }

    #[test]
    fn horizontal_prediction_copies_the_left_column() {
        let left: Vec<u8> = (0..16).map(|i| (i * 10) as u8).collect();
        let mut out = [0u8; 256];
        predict_block(H_PRED, 16, Some(&[50u8; 16]), Some(&left), 200, &mut out);
        for r in 0..16 {
            assert!(out[r * 16..r * 16 + 16].iter().all(|&p| p == left[r]));
        }
    }

    #[test]
    fn truemotion_propagates_from_the_corner() {
        // X[r][c] = clip_pixel8(L[r] + A[c] - P).
        let above = [10u8, 20, 30, 40];
        let left = [100u8, 110, 120, 130];
        let p = 50i32;
        let mut out = [0u8; 16];
        predict_block(TM_PRED, 4, Some(&above), Some(&left), p as u8, &mut out);
        for r in 0..4 {
            for c in 0..4 {
                let expect = clip_pixel8(i32::from(left[r]) + i32::from(above[c]) - p);
                assert_eq!(out[r * 4 + c], expect, "TM at ({r},{c})");
            }
        }
    }

    #[test]
    fn off_frame_edges_use_127_and_129() {
        let mut out = [0u8; 256];
        predict_block(V_PRED, 16, None, Some(&[5u8; 16]), 0, &mut out);
        assert!(
            out.iter().all(|&p| p == 127),
            "vertical off the top row fills 127"
        );
        predict_block(H_PRED, 16, Some(&[5u8; 16]), None, 0, &mut out);
        assert!(
            out.iter().all(|&p| p == 129),
            "horizontal off the left column fills 129"
        );
    }

    #[test]
    fn subblock_dc_averages_the_eight_neighbors() {
        let a = [10u8, 20, 30, 40, 0, 0, 0, 0]; // above-right is unused by B_DC_PRED
        let l = [50u8, 60, 70, 80];
        let out = subblock_predict(B_DC_PRED, &a, &l, 99);
        let v = ((10 + 20 + 30 + 40 + 50 + 60 + 70 + 80 + 4) >> 3) as u8;
        assert!(out.iter().all(|&p| p == v));
    }

    #[test]
    fn subblock_tm_matches_left_plus_above_minus_corner() {
        let a = [60u8, 70, 80, 90, 0, 0, 0, 0];
        let l = [100u8, 110, 120, 130];
        let p = 50u8;
        let out = subblock_predict(B_TM_PRED, &a, &l, p);
        for r in 0..4 {
            for c in 0..4 {
                let want = clip_pixel8(i32::from(l[r]) + i32::from(a[c]) - i32::from(p));
                assert_eq!(out[r * 4 + c], want, "TM at ({r},{c})");
            }
        }
    }

    #[test]
    fn subblock_ve_smooths_the_above_row() {
        // B[*][c] = avg3p(A + c) = (A[c-1] + 2*A[c] + A[c+1] + 2) >> 2, with A[-1] the corner.
        let a = [10u8, 20, 30, 40, 50, 0, 0, 0];
        let out = subblock_predict(B_VE_PRED, &a, &[0u8; 4], 5);
        let ext = [5i32, 10, 20, 30, 40, 50]; // [corner, A0..A4]
        for c in 0..4 {
            let want = ((ext[c] + 2 * ext[c + 1] + ext[c + 2] + 2) >> 2) as u8;
            for r in 0..4 {
                assert_eq!(out[r * 4 + c], want, "VE col {c}");
            }
        }
    }

    #[test]
    fn kf_bmode_prob_table_shape_and_corners() {
        assert_eq!(KF_BMODE_PROB.len(), NUM_BMODES);
        assert_eq!(KF_BMODE_PROB[0].len(), NUM_BMODES);
        assert_eq!(KF_BMODE_PROB[0][0].len(), 9);
        // Corner entries from RFC 6386 §11.5.
        assert_eq!(KF_BMODE_PROB[0][0][0], 231);
        assert_eq!(KF_BMODE_PROB[9][9], [112, 19, 12, 61, 195, 128, 48, 4, 24]);
        assert_eq!(BMODE_TREE.len(), 2 * (NUM_BMODES - 1));
    }
}
