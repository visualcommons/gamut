//! Executable laws for this crate's bounded inflate, [`inflate_zlib`].
//!
//! Each law takes plain data, checks one claim, and returns a [`Violation`] naming what broke it.
//! Nothing here panics on any input, so the same body runs under a pinned-seed property test in
//! the per-PR gate (`inflate.rs`, `tests::cap_law`) and under the fuzz tier's
//! `deflate_inflate_zlib` target (`tooling/gamut-fuzz`, `docs/testing.md`).
//!
//! **Output cap:** [`inflation_stays_within_cap`] inflates attacker-controlled bytes under a cap
//! and holds the result to the function's contract: an `Ok` never exceeds the cap, and the cap is
//! exact — the same stream inflates to the same bytes under a cap of exactly its length and is
//! refused as over-size under one byte less — while every `Err` is one of the two documented
//! refusals, carrying the caller's origin.
//!
//! This module is the test oracle, not the system under test, so `.cargo/mutants.toml` excludes
//! `crates/*/src/invariants.rs` from mutation.

use gamut_core::{Error, ErrorKind};

use crate::inflate_zlib;

/// The refusal [`inflate_zlib`] returns for a stream that would inflate past its cap.
const OVER_CAP: &str = "zlib stream inflates past the expected size";

/// The refusal [`inflate_zlib`] returns for every other malformed stream.
const CORRUPT: &str = "corrupt zlib stream";

/// The error origin every inflation here is attributed to.
pub const ORIGIN: &str = "gamut-deflate-invariants";

/// A law that did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    law: &'static str,
    detail: String,
}

impl Violation {
    fn new(law: &'static str, detail: String) -> Self {
        Self { law, detail }
    }

    /// The name of the law that was broken.
    #[must_use]
    pub fn law(&self) -> &'static str {
        self.law
    }

    /// What broke it, in terms the failing case can be read from.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl core::fmt::Display for Violation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.law, self.detail)
    }
}

const LAW: &str = "inflation_stays_within_cap";

/// Checks that `error` is one of [`inflate_zlib`]'s two documented refusals under [`ORIGIN`],
/// returning its message.
fn documented_refusal(error: &Error, cap: usize) -> Result<&'static str, Violation> {
    let message = error.static_message();
    let documented = error.kind() == ErrorKind::InvalidInput
        && error.origin() == Some(ORIGIN)
        && matches!(message, Some(OVER_CAP | CORRUPT));
    match message {
        Some(message) if documented => Ok(message),
        _ => Err(Violation::new(
            LAW,
            format!("cap {cap}: undocumented refusal {error:?}"),
        )),
    }
}

/// Inflates `data` under `max_out` and holds the outcome to [`inflate_zlib`]'s contract.
///
/// - An `Ok` output is at most `max_out` bytes long.
/// - The cap is exact: an output of `n` bytes is reproduced byte for byte under a cap of exactly
///   `n`, and, where `n > 0`, refused under `n - 1` as over the cap rather than as corrupt.
/// - Every `Err` is an [`ErrorKind::InvalidInput`] error under [`ORIGIN`] carrying one of the
///   two documented messages.
///
/// # Errors
///
/// Returns the [`Violation`] naming the first clause that did not hold.
pub fn inflation_stays_within_cap(data: &[u8], max_out: usize) -> Result<(), Violation> {
    let out = match inflate_zlib(data, max_out, ORIGIN) {
        Ok(out) => out,
        Err(error) => return documented_refusal(&error, max_out).map(drop),
    };
    let n = out.len();
    if n > max_out {
        return Err(Violation::new(
            LAW,
            format!("cap {max_out}: inflated {n} bytes"),
        ));
    }
    match inflate_zlib(data, n, ORIGIN) {
        Ok(exact) if exact == out => {}
        Ok(exact) => {
            return Err(Violation::new(
                LAW,
                format!(
                    "cap {n}: inflated {} bytes differing from cap {max_out}'s",
                    exact.len()
                ),
            ));
        }
        Err(error) => {
            return Err(Violation::new(
                LAW,
                format!("cap {n}: refused a stream cap {max_out} inflated to {n} bytes: {error:?}"),
            ));
        }
    }
    let Some(short) = n.checked_sub(1) else {
        return Ok(());
    };
    match inflate_zlib(data, short, ORIGIN) {
        Err(error) if documented_refusal(&error, short)? == OVER_CAP => Ok(()),
        outcome => Err(Violation::new(
            LAW,
            format!("cap {short}: a {n}-byte stream gave {outcome:?}, not the over-cap refusal"),
        )),
    }
}
