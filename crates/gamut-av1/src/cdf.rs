//! Default CDF tables (AV1 §9.4), the scans (§9.2), the constant context-offset tables (§8.3.2),
//! and the [`CdfContext`] that adapts a copy of those defaults while a tile is coded.
//!
//! Only the slices the encoder touches are included. CDFs are stored in the spec's cumulative form
//! (rising to 32768) with the trailing adaptation-count element dropped, since
//! [`gamut_bitstream::SymbolEncoder`] carries that counter out of band. These values are extracted
//! verbatim from the specification.
//!
//! The `static` tables are the *defaults*: `init_non_coeff_cdfs()`/`init_coeff_cdfs()` run for
//! every frame (`primary_ref_frame == PRIMARY_REF_NONE`), and [`CdfContext::new`] is that step.
//! The frame signals `disable_cdf_update = 0`, so each symbol coded through [`encode`] nudges its
//! CDF toward the value just coded (§8.2.6) and a decoder applies the identical update.
//!
//! One submodule per concern, all re-exported here so callers name `cdf::X` whichever owns it:
//! [`defaults`] holds the §9.4 default CDFs and the other constant intra tables (smooth weights,
//! filter-intra taps, the directional angle tables), [`scan`] the §9.2 scan orders and the §8.3.2
//! coefficient context offsets, and [`context`] the adapting per-tile state.

mod context;
mod defaults;
mod scan;

pub use context::*;
pub use defaults::*;
pub use scan::*;
