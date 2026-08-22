//! VP8 DCT/WHT coefficient token coding (RFC 6386 §13).
//!
//! Each 4×4 block's quantized coefficients are coded, in zig-zag-free natural order, as a sequence of
//! tree-coded [`Token`]s over the boolean entropy coder ([`super::bool_coder`]): magnitudes 0–4 are
//! literal, `Category1..=Category6` add a category base plus fixed-probability extra bits (§13.2), and
//! `EndOfBlock` terminates the block. The token tree probability used at each step is selected by a
//! `[plane][band][complexity]` context (§13.3) into the [`CoeffProbs`] table (default values in
//! §13.5).
//!
//! [`encode_block`] / [`decode_block`] code one block given its plane and neighbor-complexity context;
//! the macroblock-level bookkeeping of which neighbors have coefficients (§13.3) belongs to the
//! reconstruction loop (P7), so these take the context as a parameter and report whether the block
//! ended up with any non-zero coefficient. Tracked in `../STATUS.md` section K.

use super::bool_coder::{BoolDecoder, BoolEncoder, Prob, Tree};

/// Token-tree leaf value for an explicit zero coefficient (`DCT_0`).
const DCT_0: usize = 0;
/// Token-tree leaf value of the first extra-bits category (`dct_cat1`).
const DCT_CAT1: usize = 5;
/// Token-tree leaf value of end-of-block (`dct_eob`).
const DCT_EOB: usize = 11;

/// Number of plane types (RFC 6386 §13.3): Y-after-Y2, Y2, UV, Y-without-Y2.
pub const PLANE_TYPES: usize = 4;
/// Number of coefficient bands (RFC 6386 §13.3).
pub const COEFF_BANDS: usize = 8;
/// Interior-node probabilities per token-tree context (`num_dct_tokens - 1`).
pub const ENTROPY_NODES: usize = 11;

/// The coefficient token probability table, indexed `[plane][band][complexity][tree_node]`
/// (RFC 6386 §13.3). A frame starts from [`DEFAULT_COEFF_PROBS`] and may update entries (P14).
pub type CoeffProbs = [[[[Prob; ENTROPY_NODES]; 3]; COEFF_BANDS]; PLANE_TYPES];

/// The coefficient token tree (RFC 6386 §13.2 `coeff_tree`). Each pair is an interior node's `0`/`1`
/// branches; a non-positive entry `-v` is the leaf token `v` (`-11` = `dct_eob`, `0` = `DCT_0`, …).
#[rustfmt::skip]
const COEFF_TREE: &Tree = &[
    -11,  2,   0,  4,  -1,  6,   8, 12,
     -2, 10,  -3, -4,  14, 16,  -5, -6,
     18, 20,  -7, -8,  -9, -10,
];

/// Maps a coefficient position (0..16) to its band (RFC 6386 §13.3 `coeff_bands`).
const COEFF_BANDS_MAP: [usize; 16] = [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7];

/// The zig-zag scan order (RFC 6386 §13.1, §20): coefficients are coded in this order, so scan
/// position `i` carries the raster-order coefficient `ZIGZAG[i]` of the 4×4 block.
const ZIGZAG: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// Base absolute value for each extra-bits category cat1..cat6 (RFC 6386 §13.3 `categoryBase`).
const CATEGORY_BASE: [i32; 6] = [5, 7, 11, 19, 35, 67];

/// Extra-bit probabilities `Pcat1..Pcat6` (RFC 6386 §13.2); the length of each is the bit count.
const PCAT: [&[Prob]; 6] = [
    &[159],
    &[165, 145],
    &[173, 148, 140],
    &[176, 155, 140, 135],
    &[180, 157, 141, 134, 130],
    &[254, 254, 243, 230, 196, 177, 153, 140, 133, 130, 129],
];

/// The default token probabilities for a key frame (RFC 6386 §13.5).
#[rustfmt::skip]
pub const DEFAULT_COEFF_PROBS: CoeffProbs = [
    [
        [
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
        [
            [253, 136, 254, 255, 228, 219, 128, 128, 128, 128, 128],
            [189, 129, 242, 255, 227, 213, 255, 219, 128, 128, 128],
            [106, 126, 227, 252, 214, 209, 255, 255, 128, 128, 128],
        ],
        [
            [1, 98, 248, 255, 236, 226, 255, 255, 128, 128, 128],
            [181, 133, 238, 254, 221, 234, 255, 154, 128, 128, 128],
            [78, 134, 202, 247, 198, 180, 255, 219, 128, 128, 128],
        ],
        [
            [1, 185, 249, 255, 243, 255, 128, 128, 128, 128, 128],
            [184, 150, 247, 255, 236, 224, 128, 128, 128, 128, 128],
            [77, 110, 216, 255, 236, 230, 128, 128, 128, 128, 128],
        ],
        [
            [1, 101, 251, 255, 241, 255, 128, 128, 128, 128, 128],
            [170, 139, 241, 252, 236, 209, 255, 255, 128, 128, 128],
            [37, 116, 196, 243, 228, 255, 255, 255, 128, 128, 128],
        ],
        [
            [1, 204, 254, 255, 245, 255, 128, 128, 128, 128, 128],
            [207, 160, 250, 255, 238, 128, 128, 128, 128, 128, 128],
            [102, 103, 231, 255, 211, 171, 128, 128, 128, 128, 128],
        ],
        [
            [1, 152, 252, 255, 240, 255, 128, 128, 128, 128, 128],
            [177, 135, 243, 255, 234, 225, 128, 128, 128, 128, 128],
            [80, 129, 211, 255, 194, 224, 128, 128, 128, 128, 128],
        ],
        [
            [1, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [246, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [255, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
    ],
    [
        [
            [198, 35, 237, 223, 193, 187, 162, 160, 145, 155, 62],
            [131, 45, 198, 221, 172, 176, 220, 157, 252, 221, 1],
            [68, 47, 146, 208, 149, 167, 221, 162, 255, 223, 128],
        ],
        [
            [1, 149, 241, 255, 221, 224, 255, 255, 128, 128, 128],
            [184, 141, 234, 253, 222, 220, 255, 199, 128, 128, 128],
            [81, 99, 181, 242, 176, 190, 249, 202, 255, 255, 128],
        ],
        [
            [1, 129, 232, 253, 214, 197, 242, 196, 255, 255, 128],
            [99, 121, 210, 250, 201, 198, 255, 202, 128, 128, 128],
            [23, 91, 163, 242, 170, 187, 247, 210, 255, 255, 128],
        ],
        [
            [1, 200, 246, 255, 234, 255, 128, 128, 128, 128, 128],
            [109, 178, 241, 255, 231, 245, 255, 255, 128, 128, 128],
            [44, 130, 201, 253, 205, 192, 255, 255, 128, 128, 128],
        ],
        [
            [1, 132, 239, 251, 219, 209, 255, 165, 128, 128, 128],
            [94, 136, 225, 251, 218, 190, 255, 255, 128, 128, 128],
            [22, 100, 174, 245, 186, 161, 255, 199, 128, 128, 128],
        ],
        [
            [1, 182, 249, 255, 232, 235, 128, 128, 128, 128, 128],
            [124, 143, 241, 255, 227, 234, 128, 128, 128, 128, 128],
            [35, 77, 181, 251, 193, 211, 255, 205, 128, 128, 128],
        ],
        [
            [1, 157, 247, 255, 236, 231, 255, 255, 128, 128, 128],
            [121, 141, 235, 255, 225, 227, 255, 255, 128, 128, 128],
            [45, 99, 188, 251, 195, 217, 255, 224, 128, 128, 128],
        ],
        [
            [1, 1, 251, 255, 213, 255, 128, 128, 128, 128, 128],
            [203, 1, 248, 255, 255, 128, 128, 128, 128, 128, 128],
            [137, 1, 177, 255, 224, 255, 128, 128, 128, 128, 128],
        ],
    ],
    [
        [
            [253, 9, 248, 251, 207, 208, 255, 192, 128, 128, 128],
            [175, 13, 224, 243, 193, 185, 249, 198, 255, 255, 128],
            [73, 17, 171, 221, 161, 179, 236, 167, 255, 234, 128],
        ],
        [
            [1, 95, 247, 253, 212, 183, 255, 255, 128, 128, 128],
            [239, 90, 244, 250, 211, 209, 255, 255, 128, 128, 128],
            [155, 77, 195, 248, 188, 195, 255, 255, 128, 128, 128],
        ],
        [
            [1, 24, 239, 251, 218, 219, 255, 205, 128, 128, 128],
            [201, 51, 219, 255, 196, 186, 128, 128, 128, 128, 128],
            [69, 46, 190, 239, 201, 218, 255, 228, 128, 128, 128],
        ],
        [
            [1, 191, 251, 255, 255, 128, 128, 128, 128, 128, 128],
            [223, 165, 249, 255, 213, 255, 128, 128, 128, 128, 128],
            [141, 124, 248, 255, 255, 128, 128, 128, 128, 128, 128],
        ],
        [
            [1, 16, 248, 255, 255, 128, 128, 128, 128, 128, 128],
            [190, 36, 230, 255, 236, 255, 128, 128, 128, 128, 128],
            [149, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
        [
            [1, 226, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [247, 192, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [240, 128, 255, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
        [
            [1, 134, 252, 255, 255, 128, 128, 128, 128, 128, 128],
            [213, 62, 250, 255, 255, 128, 128, 128, 128, 128, 128],
            [55, 93, 255, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
        [
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
    ],
    [
        [
            [202, 24, 213, 235, 186, 191, 220, 160, 240, 175, 255],
            [126, 38, 182, 232, 169, 184, 228, 174, 255, 187, 128],
            [61, 46, 138, 219, 151, 178, 240, 170, 255, 216, 128],
        ],
        [
            [1, 112, 230, 250, 199, 191, 247, 159, 255, 255, 128],
            [166, 109, 228, 252, 211, 215, 255, 174, 128, 128, 128],
            [39, 77, 162, 232, 172, 180, 245, 178, 255, 255, 128],
        ],
        [
            [1, 52, 220, 246, 198, 199, 249, 220, 255, 255, 128],
            [124, 74, 191, 243, 183, 193, 250, 221, 255, 255, 128],
            [24, 71, 130, 219, 154, 170, 243, 182, 255, 255, 128],
        ],
        [
            [1, 182, 225, 249, 219, 240, 255, 224, 128, 128, 128],
            [149, 150, 226, 252, 216, 205, 255, 171, 128, 128, 128],
            [28, 108, 170, 242, 183, 194, 254, 223, 255, 255, 128],
        ],
        [
            [1, 81, 230, 252, 204, 203, 255, 192, 128, 128, 128],
            [123, 102, 209, 247, 188, 196, 255, 233, 128, 128, 128],
            [20, 95, 153, 243, 164, 173, 255, 203, 128, 128, 128],
        ],
        [
            [1, 222, 248, 255, 216, 213, 128, 128, 128, 128, 128],
            [168, 175, 246, 252, 235, 205, 255, 255, 128, 128, 128],
            [47, 116, 215, 255, 211, 212, 255, 255, 128, 128, 128],
        ],
        [
            [1, 121, 236, 253, 212, 214, 255, 255, 128, 128, 128],
            [141, 84, 213, 252, 201, 202, 255, 219, 128, 128, 128],
            [42, 80, 160, 240, 162, 185, 255, 205, 128, 128, 128],
        ],
        [
            [1, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [244, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [238, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
    ],
];

/// The constant probabilities gating each coefficient-probability update flag (RFC 6386 §13.4).
#[rustfmt::skip]
pub const COEFF_UPDATE_PROBS: CoeffProbs = [
    [
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [176, 246, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [223, 241, 252, 255, 255, 255, 255, 255, 255, 255, 255],
            [249, 253, 253, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 244, 252, 255, 255, 255, 255, 255, 255, 255, 255],
            [234, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [253, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 246, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [239, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 248, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [251, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [251, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 253, 255, 254, 255, 255, 255, 255, 255, 255],
            [250, 255, 254, 255, 254, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
    ],
    [
        [
            [217, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [225, 252, 241, 253, 255, 255, 254, 255, 255, 255, 255],
            [234, 250, 241, 250, 253, 255, 253, 254, 255, 255, 255],
        ],
        [
            [255, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [223, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [238, 253, 254, 254, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 248, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [249, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 253, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [247, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [252, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [253, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [250, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
    ],
    [
        [
            [186, 251, 250, 255, 255, 255, 255, 255, 255, 255, 255],
            [234, 251, 244, 254, 255, 255, 255, 255, 255, 255, 255],
            [251, 251, 243, 253, 254, 255, 254, 255, 255, 255, 255],
        ],
        [
            [255, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [236, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [251, 253, 253, 254, 254, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
    ],
    [
        [
            [248, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [250, 254, 252, 254, 255, 255, 255, 255, 255, 255, 255],
            [248, 254, 249, 253, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 253, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [246, 253, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [252, 254, 251, 254, 254, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 252, 255, 255, 255, 255, 255, 255, 255, 255],
            [248, 254, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [253, 255, 254, 254, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 251, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [245, 251, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [253, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 251, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [252, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 252, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [249, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [250, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
    ],
];

/// Reads the per-frame coefficient-probability update record (RFC 6386 §13.4), applying any updates
/// in place to `probs`. Each entry is gated by a bool at its [`COEFF_UPDATE_PROBS`] probability; a set
/// flag is followed by the replacement 8-bit probability.
pub fn read_coeff_prob_updates(dec: &mut BoolDecoder, probs: &mut CoeffProbs) {
    let flat_update = COEFF_UPDATE_PROBS.iter().flatten().flatten().flatten();
    let flat_probs = probs.iter_mut().flatten().flatten().flatten();
    for (&update_prob, prob) in flat_update.zip(flat_probs) {
        if dec.get_bool(update_prob) {
            *prob = dec.get_literal(8) as Prob;
        }
    }
}

/// Writes the coefficient-probability update record (RFC 6386 §13.4): for each entry, a flag (at its
/// [`COEFF_UPDATE_PROBS`] probability) marking whether `new` differs from `old`, followed by the new
/// 8-bit probability when it does. Passing equal tables writes the all-"no-update" record that a
/// minimal key-frame header carries.
pub fn write_coeff_prob_updates(enc: &mut BoolEncoder, new: &CoeffProbs, old: &CoeffProbs) {
    let flat_update = COEFF_UPDATE_PROBS.iter().flatten().flatten().flatten();
    let flat_new = new.iter().flatten().flatten().flatten();
    let flat_old = old.iter().flatten().flatten().flatten();
    for (&update_prob, (&n, &o)) in flat_update.zip(flat_new.zip(flat_old)) {
        enc.put_bool(update_prob, n != o);
        if n != o {
            enc.put_literal(u32::from(n), 8);
        }
    }
}

/// First coded coefficient index for a plane: Y-after-Y2 (plane 0) skips coefficient 0 (its DC comes
/// from the Y2 block); every other plane starts at 0.
#[must_use]
pub fn first_coeff(plane: usize) -> usize {
    usize::from(plane == 0)
}

/// Maps a non-negative absolute level to its token-tree leaf value (RFC 6386 §13.2).
fn token_for_abs(abs: i32) -> usize {
    match abs {
        0..=4 => abs as usize,
        5..=6 => DCT_CAT1,
        7..=10 => 6,
        11..=18 => 7,
        19..=34 => 8,
        35..=66 => 9,
        _ => 10,
    }
}

/// Decodes a category token's extra bits, MSB first, into the unsigned offset (RFC 6386 §13.2
/// `DCTextra`).
fn decode_extra(dec: &mut BoolDecoder, probs: &[Prob]) -> i32 {
    let mut v = 0i32;
    for &p in probs {
        v = v + v + i32::from(dec.get_bool(p));
    }
    v
}

/// Decodes one 4×4 block's coefficients into `coeffs` from `dec`, using `probs` and the
/// neighbor-complexity context `nonzero_ctx` (the number, 0..=2, of same-plane above/left neighbor
/// blocks with coefficients). Positions `[first_coeff(plane)..16]` are overwritten; lower positions
/// (the externally supplied Y2-derived DC for plane 0) are left untouched. Returns whether the block
/// has any non-zero coefficient.
pub fn decode_block(
    dec: &mut BoolDecoder,
    coeffs: &mut [i16; 16],
    plane: usize,
    nonzero_ctx: usize,
    probs: &CoeffProbs,
) -> bool {
    let first = first_coeff(plane);
    coeffs[first..].fill(0);
    let mut ctx3 = nonzero_ctx;
    let mut prev_zero = false;
    let mut has_coeffs = false;
    let mut i = first;
    while i < 16 {
        let band = COEFF_BANDS_MAP[i];
        let node_probs = &probs[plane][band][ctx3];
        let token = if prev_zero {
            dec.get_tree_start(COEFF_TREE, node_probs, 2)
        } else {
            dec.get_tree(COEFF_TREE, node_probs)
        };
        if token == DCT_EOB {
            break;
        }
        let abs = if token == DCT_0 {
            0
        } else {
            has_coeffs = true;
            if token >= DCT_CAT1 {
                CATEGORY_BASE[token - DCT_CAT1] + decode_extra(dec, PCAT[token - DCT_CAT1])
            } else {
                token as i32
            }
        };
        if token != DCT_0 {
            coeffs[ZIGZAG[i]] = if dec.get_flag() {
                -(abs as i16)
            } else {
                abs as i16
            };
        }
        ctx3 = complexity(abs);
        prev_zero = token == DCT_0;
        i += 1;
    }
    has_coeffs
}

/// A consumer of one block's coded bits, letting the tokenization in [`walk_block`] be written once
/// and reused for encoding, counting, and costing.
///
/// The split matters because the two kinds of bit are governed differently: the tree bits are the
/// ones [`CoeffProbs`] covers and a two-pass encoder can re-derive, while the extra-bit and sign
/// bits ride fixed probabilities the frame header cannot update.
pub trait BlockSink {
    /// One tree-coded bit, at `probs[plane][band][ctx][node]` of the frame's coefficient
    /// probabilities.
    fn tree_bit(&mut self, plane: usize, band: usize, ctx: usize, node: usize, bit: bool);
    /// One bit at a fixed probability: a category's extra bits, or a coefficient's sign.
    fn fixed_bit(&mut self, prob: Prob, bit: bool);
}

/// A [`BlockSink`] that writes to a [`BoolEncoder`] under a given probability table.
struct WriteSink<'a, 'p> {
    enc: &'a mut BoolEncoder,
    probs: &'p CoeffProbs,
}

impl BlockSink for WriteSink<'_, '_> {
    fn tree_bit(&mut self, plane: usize, band: usize, ctx: usize, node: usize, bit: bool) {
        self.enc.put_bool(self.probs[plane][band][ctx][node], bit);
    }

    fn fixed_bit(&mut self, prob: Prob, bit: bool) {
        self.enc.put_bool(prob, bit);
    }
}

/// Per-context counts of the zero and one branches taken by a frame's coefficient tokens — the
/// input to the probability optimizer.
///
/// Indexed exactly like [`CoeffProbs`], with a final `[zeros, ones]` pair.
pub type CoeffCounts = [[[[[u32; 2]; ENTROPY_NODES]; 3]; COEFF_BANDS]; PLANE_TYPES];

/// A [`BlockSink`] that tallies tree bits instead of writing them. Fixed-probability bits are
/// ignored: no probability update can change their cost, so they are not part of the optimization.
struct CountSink<'a> {
    counts: &'a mut CoeffCounts,
}

impl BlockSink for CountSink<'_> {
    fn tree_bit(&mut self, plane: usize, band: usize, ctx: usize, node: usize, bit: bool) {
        self.counts[plane][band][ctx][node][usize::from(bit)] += 1;
    }

    fn fixed_bit(&mut self, _prob: Prob, _bit: bool) {}
}

/// Walks one 4×4 block's coefficient tokens, handing every coded bit to `sink` (RFC 6386 §13).
/// Returns whether the block has any non-zero coefficient.
///
/// This is the single definition of the tokenization; [`encode_block`] and [`count_block`] are thin
/// wrappers that differ only in what they do with the bits.
pub fn walk_block(
    coeffs: &[i16; 16],
    plane: usize,
    nonzero_ctx: usize,
    sink: &mut impl BlockSink,
) -> bool {
    let first = first_coeff(plane);
    // One past the last non-zero coefficient: tokens are coded for [first, eob), then EOB (if eob<16).
    let eob = (first..16)
        .rev()
        .find(|&i| coeffs[ZIGZAG[i]] != 0)
        .map_or(first, |i| i + 1);
    let mut ctx3 = nonzero_ctx;
    let mut prev_zero = false;
    for i in first..eob {
        let level = i32::from(coeffs[ZIGZAG[i]]);
        let abs = level.abs();
        let token = token_for_abs(abs);
        let band = COEFF_BANDS_MAP[i];
        // After a zero token the tree is entered at node 2 — the `DCT_0` branch cannot repeat.
        let start = if prev_zero { 2 } else { 0 };
        super::bool_coder::walk_tree(COEFF_TREE, token, start, |node, bit| {
            sink.tree_bit(plane, band, ctx3, node, bit);
        });
        if token != DCT_0 {
            if token >= DCT_CAT1 {
                let probs = PCAT[token - DCT_CAT1];
                let offset = abs - CATEGORY_BASE[token - DCT_CAT1];
                let n = probs.len();
                for (j, &p) in probs.iter().enumerate() {
                    sink.fixed_bit(p, (offset >> (n - 1 - j)) & 1 != 0);
                }
            }
            sink.fixed_bit(128, level < 0);
        }
        ctx3 = complexity(abs);
        prev_zero = token == DCT_0;
    }
    if eob < 16 {
        // The coefficient at `eob - 1` (if any) is non-zero, so an EOB never follows a zero here.
        let band = COEFF_BANDS_MAP[eob];
        super::bool_coder::walk_tree(COEFF_TREE, DCT_EOB, 0, |node, bit| {
            sink.tree_bit(plane, band, ctx3, node, bit);
        });
    }
    eob > first
}

/// Encodes one 4×4 block's coefficients to `enc`, mirroring [`decode_block`]. Returns whether the
/// block has any non-zero coefficient.
pub fn encode_block(
    enc: &mut BoolEncoder,
    coeffs: &[i16; 16],
    plane: usize,
    nonzero_ctx: usize,
    probs: &CoeffProbs,
) -> bool {
    walk_block(coeffs, plane, nonzero_ctx, &mut WriteSink { enc, probs })
}

/// Tallies one 4×4 block's tree-coded bits into `counts` without writing anything. Returns whether
/// the block has any non-zero coefficient.
pub fn count_block(
    counts: &mut CoeffCounts,
    coeffs: &[i16; 16],
    plane: usize,
    nonzero_ctx: usize,
) -> bool {
    walk_block(coeffs, plane, nonzero_ctx, &mut CountSink { counts })
}

/// The next-coefficient complexity context from an absolute level: 0 for zero, 1 for ±1, 2 otherwise
/// (RFC 6386 §13.3).
fn complexity(abs: i32) -> usize {
    match abs {
        0 => 0,
        1 => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
    }

    /// Round-trips a block through encode/decode and asserts equality on the coded positions.
    fn check_roundtrip(coeffs: &[i16; 16], plane: usize, nonzero_ctx: usize) {
        let mut enc = BoolEncoder::new();
        let enc_has = encode_block(&mut enc, coeffs, plane, nonzero_ctx, &DEFAULT_COEFF_PROBS);
        let bytes = enc.finish();

        let mut dec = BoolDecoder::new(&bytes);
        let mut out = [0i16; 16];
        let dec_has = decode_block(&mut dec, &mut out, plane, nonzero_ctx, &DEFAULT_COEFF_PROBS);

        let first = first_coeff(plane);
        assert_eq!(
            &out[first..],
            &coeffs[first..],
            "plane {plane} ctx {nonzero_ctx}"
        );
        assert_eq!(enc_has, dec_has, "has-coeffs disagreement");
        assert_eq!(enc_has, coeffs[first..].iter().any(|&c| c != 0));
    }

    #[test]
    fn default_table_shape_and_checksum() {
        // Guards the §13.5 transcription: a wrong/missing value changes the sum.
        let sum: u32 = DEFAULT_COEFF_PROBS
            .iter()
            .flatten()
            .flatten()
            .flatten()
            .map(|&p| u32::from(p))
            .sum();
        assert_eq!(sum, 174918);
    }

    #[test]
    fn empty_block_round_trips() {
        for plane in 0..4 {
            for ctx in 0..3 {
                check_roundtrip(&[0i16; 16], plane, ctx);
            }
        }
    }

    #[test]
    fn dc_only_and_full_blocks() {
        let mut dc = [0i16; 16];
        dc[0] = 7; // plane 0 ignores index 0, so test it on the other planes
        for plane in [1usize, 2, 3] {
            check_roundtrip(&dc, plane, 1);
        }
        let full = [5i16; 16]; // every position non-zero -> no EOB coded
        for plane in 0..4 {
            check_roundtrip(&full, plane, 2);
        }
    }

    #[test]
    fn interior_zeros_exercise_eob_skip() {
        // A zero between non-zeros forces the "EOB cannot follow a zero" tree-start path.
        let mut b = [0i16; 16];
        b[1] = 3;
        b[2] = 0;
        b[3] = -1;
        b[7] = 0;
        b[8] = 12;
        for plane in 0..4 {
            check_roundtrip(&b, plane, 0);
        }
    }

    #[test]
    fn all_categories_round_trip() {
        // One representative magnitude in each category range (incl. both ends of cat6).
        let mags = [1i16, 2, 3, 4, 5, 6, 7, 10, 11, 18, 19, 34, 35, 66, 67, 2048];
        let mut b = [0i16; 16];
        for (i, &m) in mags.iter().enumerate() {
            b[i] = if i % 2 == 0 { m } else { -m };
        }
        check_roundtrip(&b, 3, 0);
    }

    #[test]
    fn fuzz_round_trip() {
        let mut rng = SplitMix64(0xc0ffee);
        for _ in 0..400 {
            let plane = (rng.next() % 4) as usize;
            let ctx = (rng.next() % 3) as usize;
            let mut b = [0i16; 16];
            // Realistic-ish: mostly small, sparse, occasionally large.
            let n = (rng.next() % 16) as usize;
            for slot in b.iter_mut().take(n) {
                let r = rng.next();
                let mag = match r % 8 {
                    0..=4 => (r >> 8) % 3,    // 0,1,2 (common)
                    5 => 5 + (r >> 8) % 62,   // cat1-2
                    6 => 67 + (r >> 8) % 200, // cat6
                    _ => (r >> 8) % 35,       // up to cat5
                } as i16;
                *slot = if r & 0x100 != 0 { -mag } else { mag };
            }
            check_roundtrip(&b, plane, ctx);
        }
    }
}
