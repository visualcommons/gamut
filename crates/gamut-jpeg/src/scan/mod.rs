//! Entropy-scan decoding (T.81 Annex F §F.2 and Annex G §G.2): the byte-destuffing bit reader
//! (§F.2.2.5), the canonical `DECODE`/`RECEIVE`/`EXTEND` procedures (Figures F.16, F.17, F.12), the
//! sequential MCU walk that reconstructs each component's sample plane (§A.2), and the progressive
//! DCT decoder (§G.2) that accumulates DCT coefficients across scans. Restart processing follows
//! §E.2.5.
//!
//! Decoding is deliberately generous: every read is bounds-checked and every table lookup is
//! fallible, so malformed input yields a typed [`Error`] and never a panic. The decoder can also
//! supply an MCU-row ceiling for a DNL-deferred sequential frame, preventing an unbounded sample
//! plane from growing before the height marker arrives.
//!
//! # Progressive coefficient model (§G.1.1)
//!
//! A progressive (SOF2) frame is coded as many scans, each carrying one *band* of the zig-zag
//! sequence at one *successive-approximation* precision. The decoder therefore cannot reconstruct a
//! block from a single scan; instead each frame component owns a full `i32` DCT-coefficient buffer
//! ([`ProgComp`]) at block granularity that the scans fill incrementally. Only after **all** scans
//! (at EOI) is each component dequantized (§A.3.4), inverse-transformed (§A.3.3) and level-shifted
//! (§A.3.1) — once — through the same [`write_block`] tail the sequential path uses.
//!
//! # Module layout
//!
//! One submodule per scan type over a shared reader: `reader` is the destuffing bit reader and the
//! `DECODE`/`RECEIVE`/`EXTEND` procedures both scan types decode with, `sequential` the
//! sequential-process scan (Annex F), and `progressive` the progressive-process scans (Annex G).
//! This module holds the sample [`Plane`] and the reconstruction tail ([`write_block`]) the two
//! share.

mod progressive;
mod reader;
mod sequential;

#[cfg(doc)]
use gamut_core::Error;
use gamut_dsp::jpeg::inverse_dct8x8_int;

pub use self::progressive::{ProgComp, decode_progressive_scan};
pub use self::sequential::{McuRowLimit, decode_scan};

/// One decoded component's reconstructed sample plane, stored at block-padded resolution: `stride`
/// is `blocks_per_line · 8` and rows grow in units of a block as the scan proceeds. The valid image
/// region (`comp_w × comp_h`) is cropped from this during upsampling.
pub struct Plane {
    /// Reconstructed clamped samples, row-major, `stride` wide.
    pub data: Vec<u8>,
    /// Row length in samples (`blocks_per_line · 8`).
    pub stride: usize,
    /// Horizontal sampling factor of the component (for later upsampling).
    pub h: u8,
    /// Vertical sampling factor of the component (for later upsampling).
    pub v: u8,
}

impl Plane {
    /// Grows the backing store so at least `rows` pixel rows exist (zero-filled). Written as a
    /// resize to `max(len, want)` — never a shrink — so there is no "already big enough" comparison
    /// whose boundary would be unobservable (a same-length resize is a no-op).
    fn ensure_rows(&mut self, rows: usize) {
        let want = rows * self.stride;
        self.data.resize(self.data.len().max(want), 0);
    }
}

/// Level-shifts (+128) and clamps one reconstructed sample to `0..=255` (§A.3.1). The saturating
/// add keeps the shift in range for any `i32` the kernel can return.
///
/// Written as shift-then-clamp, not `(s.clamp(-128, 127) + 128) as u8`: in that form the cast
/// wraps `s - 128` onto the same byte as `s + 128`, so the `+`→`-` mutant had an equivalent twin
/// no input could kill (#896's mutation survey). This form leaves no operator to mutate here; the
/// function stays covered by its whole-body mutants.
fn level_shift(s: i32) -> u8 {
    s.saturating_add(128).clamp(0, 255) as u8
}

/// Inverse-transforms a dequantized natural-order block `zz`, level-shifts (+128) and clamps each
/// sample to `0..=255`, and writes the 8×8 result into `plane` at block position `(bx, by)`
/// (§A.3.1). The single reconstruction tail shared by the sequential (`Ctx::decode_block`) and
/// progressive ([`ProgComp::into_plane`]) paths, so both produce byte-identical samples.
fn write_block(zz: &mut [i32; 64], plane: &mut Plane, bx: usize, by: usize) {
    inverse_dct8x8_int(zz);
    plane.ensure_rows((by + 1) * 8);
    let stride = plane.stride;
    for (row, samples) in zz.as_chunks::<8>().0.iter().enumerate() {
        let dst = (by * 8 + row) * stride + bx * 8;
        for (out, &s) in plane.data[dst..dst + 8].iter_mut().zip(samples) {
            *out = level_shift(s);
        }
    }
}

/// Writes the flat 8×8 block whose every level-shifted sample is `s` into `plane` at block
/// position `(bx, by)` — what [`write_block`] writes for a DC-only block, without the transform.
fn write_flat(s: i32, plane: &mut Plane, bx: usize, by: usize) {
    plane.ensure_rows((by + 1) * 8);
    let stride = plane.stride;
    let value = level_shift(s);
    for row in 0..8 {
        let dst = (by * 8 + row) * stride + bx * 8;
        plane.data[dst..dst + 8].fill(value);
    }
}
