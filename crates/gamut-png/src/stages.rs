//! The encoder's pipeline stages, exposed so they can be timed one at a time (issue #224).
//!
//! A `benches/` target compiles as a separate crate, so it can only reach `pub` items — and the
//! encoder's stages are all crate-private, by design. Rather than widen the shipped API or split
//! working code apart to be reachable, this module re-exports exactly the stage entry points a
//! benchmark drives, behind the `test-support` feature.
//!
//! **No SemVer guarantee.** This is gamut's own harness, not API to pin, and it is `doc(hidden)`
//! for that reason. The `gamut` umbrella never enables the feature, so the shipped surface and
//! `mise run check-ffi-features` are unaffected.
//!
//! It is deliberately re-exports and nothing else — no wrapper bodies. A wrapper would be an
//! executable line that no gate ever runs (bench targets carry `test = false`, so neither
//! `cargo test`, `cargo llvm-cov` nor `cargo mutants` reach them), which would both drag the
//! coverage floor and generate unkillable mutants. `.cargo/mutants.toml` already states the rule
//! this follows, in its `crates/gamut/**` entry: "pure feature-gated re-exports (no function
//! bodies), so it carries no logic of its own to mutate."

pub use crate::crc32::Crc32;
pub use crate::filter::filter_image;
pub use crate::pack::pack_scanlines;
pub use crate::reduce::{Reduced, analyze8, analyze16};
