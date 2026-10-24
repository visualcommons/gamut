//! Input arm for experiment #974: the saturated content overshoot deringing acts on, which the #29
//! corpus barely carries.
//!
//! Not a test and not a benchmark: it asserts nothing. Sandboxed experiment code under the #29
//! policy -- it must keep compiling as the library moves, and it is validated only by the numbers
//! the measurement prints. `experiments/974/run.sh` (`mise run experiment 974`) drives it; the
//! verdict and the tables live in `experiments/974/RESULTS.md`.
//!
//! The measurement itself is #29's arm (`examples/experiment_29.rs`), rebuilt once per
//! `experiments/974/*.patch`: this file only writes the extra inputs it reads. One subcommand:
//!
//! - `inputs --out DIR` writes two binary PPMs (`P6`, maxval 255) into `DIR` and prints one CSV row
//!   per image, `image,width,height,white_share`, where `white_share` is the fraction of pixels
//!   whose three channels are all 255, i.e. whose JFIF luma is 255 (T.871 §7). Those are the samples
//!   deringing may raise past the ceiling.
//!   - `clipped-512x384`: the decode bench's deterministic photo-like generator
//!     (`benches/common/corpus.rs`) with every channel mapped through `2 (v - 64)`, clamped, so its
//!     highlights clip to white the way an over-exposed photograph does.
//!   - `text-384x256`: dark one- and two-pixel strokes on white, laid out as lines of glyph-sized
//!     boxes from a fixed-seed LCG: the black-text-on-white case mozjpeg's deringing targets.

use std::fmt::Write as _;
use std::path::Path;

#[allow(
    dead_code,
    reason = "the arm uses only the generator, not the bench's streams"
)]
#[path = "../benches/common/corpus.rs"]
mod corpus;

/// `corpus::photo_rgb` with each channel mapped through `2 (v - 64)`, clamped to `0..=255`.
fn clipped(w: u32, h: u32) -> Vec<u8> {
    corpus::photo_rgb(w, h)
        .into_iter()
        .map(|v| ((i32::from(v) - 64) * 2).clamp(0, 255) as u8)
        .collect()
}

/// Dark strokes on white: text lines 11 pixels high on an 18-pixel pitch, each a run of glyph boxes
/// 4-7 pixels wide drawn from left/right/top/middle/bottom bars, with a word gap after a random
/// one in five glyphs. A 32-bit LCG (Numerical Recipes constants) with a fixed seed picks every
/// choice, so the samples are a pure function of the dimensions.
fn text(w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut grey = vec![255u8; w * h];
    let mut state: u32 = 0x0974_0974;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state >> 8
    };
    let ink = 24u8;
    let fill = |grey: &mut [u8], x0: usize, y0: usize, x1: usize, y1: usize| {
        for y in y0..y1.min(h) {
            for x in x0..x1.min(w) {
                grey[y * w + x] = ink;
            }
        }
    };
    let mut top = 6;
    while top + 11 <= h - 6 {
        let mut x = 6;
        loop {
            let gw = 4 + (next() % 4) as usize;
            if x + gw > w - 6 {
                break;
            }
            let stroke = 1 + (next() % 2) as usize;
            // Bit 0 (the left bar) always set, so no glyph is blank.
            let bars = (next() % 32) | 1;
            let (bottom, mid) = (top + 11, top + 5);
            if bars & 1 != 0 {
                fill(&mut grey, x, top, x + stroke, bottom);
            }
            if bars & 2 != 0 {
                fill(&mut grey, x + gw - stroke, top, x + gw, bottom);
            }
            if bars & 4 != 0 {
                fill(&mut grey, x, top, x + gw, top + stroke);
            }
            if bars & 8 != 0 {
                fill(&mut grey, x, mid, x + gw, mid + stroke);
            }
            if bars & 16 != 0 {
                fill(&mut grey, x, bottom - stroke, x + gw, bottom);
            }
            x += gw + 2;
            if next() % 5 == 0 {
                x += 5;
            }
        }
        top += 18;
    }
    grey.into_iter().flat_map(|g| [g, g, g]).collect()
}

/// Writes `rgb` as a binary PPM and returns its CSV row.
fn write_ppm(dir: &Path, name: &str, rgb: &[u8], w: u32, h: u32) -> String {
    let mut bytes = format!("P6\n{w} {h}\n255\n").into_bytes();
    bytes.extend_from_slice(rgb);
    let path = dir.join(format!("{name}.ppm"));
    std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let px = rgb.as_chunks::<3>().0;
    let white = px.iter().filter(|p| **p == [255; 3]).count();
    let mut row = String::new();
    write!(row, "{name},{w},{h},{:.4}", white as f64 / px.len() as f64).expect("write to String");
    row
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [cmd, flag, dir] = args.as_slice() else {
        panic!("usage: experiment_974 inputs --out DIR");
    };
    assert!(
        cmd == "inputs" && flag == "--out",
        "usage: experiment_974 inputs --out DIR"
    );
    let dir = Path::new(dir);
    std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    println!("image,width,height,white_share");
    println!(
        "{}",
        write_ppm(dir, "clipped-512x384", &clipped(512, 384), 512, 384)
    );
    println!(
        "{}",
        write_ppm(dir, "text-384x256", &text(384, 256), 384, 256)
    );
}
