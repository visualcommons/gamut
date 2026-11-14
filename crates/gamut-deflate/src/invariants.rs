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
    outcomes_honour_cap(max_out, |cap| inflate_zlib(data, cap, ORIGIN))
}

/// Checks the outcomes `inflate` returns for one stream under the caps the law asks about —
/// `max_out`, then the output's own length `n`, then `n - 1` — against the law's clauses.
///
/// Split from [`inflation_stays_within_cap`] so the unit tests below can supply a deliberately
/// broken inflater and show each clause can fail.
fn outcomes_honour_cap(
    max_out: usize,
    mut inflate: impl FnMut(usize) -> Result<Vec<u8>, Error>,
) -> Result<(), Violation> {
    let out = match inflate(max_out) {
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
    match inflate(n) {
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
    match inflate(short) {
        Err(error) if documented_refusal(&error, short)? == OVER_CAP => Ok(()),
        outcome => Err(Violation::new(
            LAW,
            format!("cap {short}: a {n}-byte stream gave {outcome:?}, not the over-cap refusal"),
        )),
    }
}

#[cfg(test)]
mod tests {
    // `invariants.rs` is excluded from mutation, so these are not mutation-driven. They exist so
    // that a law which can no longer *fail* is caught: a law stuck at `Ok(())` would make every
    // property over it vacuous while still passing. Each test supplies an inflater that honours
    // the contract except in the one clause it names.

    use gamut_core::{Error, ErrorKind};

    use super::{CORRUPT, LAW, ORIGIN, OVER_CAP, Violation, outcomes_honour_cap};

    const PAYLOAD: &[u8] = b"abcd";

    /// The contract's own behaviour for a stream that inflates to [`PAYLOAD`].
    fn honest(cap: usize) -> Result<Vec<u8>, Error> {
        if cap < PAYLOAD.len() {
            Err(Error::invalid_input(ORIGIN, OVER_CAP))
        } else {
            Ok(PAYLOAD.to_vec())
        }
    }

    fn violation(
        max_out: usize,
        inflate: impl FnMut(usize) -> Result<Vec<u8>, Error>,
    ) -> Violation {
        let v = outcomes_honour_cap(max_out, inflate).unwrap_err();
        assert_eq!(v.law(), LAW);
        assert_eq!(v.to_string(), format!("{}: {}", v.law(), v.detail()));
        v
    }

    #[test]
    fn the_contract_satisfies_the_cap_law() {
        assert_eq!(outcomes_honour_cap(8, honest), Ok(()));
        assert_eq!(outcomes_honour_cap(PAYLOAD.len(), honest), Ok(()));
        assert_eq!(outcomes_honour_cap(2, honest), Ok(()));
        assert_eq!(outcomes_honour_cap(0, |_| Ok(Vec::new())), Ok(()));
        let corrupt = |_| Err(Error::invalid_input(ORIGIN, CORRUPT));
        assert_eq!(outcomes_honour_cap(8, corrupt), Ok(()));
    }

    #[test]
    fn an_output_past_the_cap_violates_the_cap_law() {
        let v = violation(2, |_| Ok(PAYLOAD.to_vec()));
        assert!(v.detail().contains("inflated 4 bytes"), "{v}");
    }

    #[test]
    fn different_bytes_under_the_exact_cap_violate_the_cap_law() {
        let v = violation(8, |cap| {
            if cap == PAYLOAD.len() {
                Ok(b"abce".to_vec())
            } else {
                honest(cap)
            }
        });
        assert!(v.detail().contains("differing"), "{v}");
    }

    #[test]
    fn a_refusal_under_the_exact_cap_violates_the_cap_law() {
        let v = violation(8, |cap| {
            if cap == PAYLOAD.len() {
                Err(Error::invalid_input(ORIGIN, OVER_CAP))
            } else {
                honest(cap)
            }
        });
        assert!(v.detail().contains("refused a stream"), "{v}");
    }

    #[test]
    fn success_one_byte_under_the_output_violates_the_cap_law() {
        let v = violation(8, |_| Ok(PAYLOAD.to_vec()));
        assert!(v.detail().contains("not the over-cap refusal"), "{v}");
    }

    #[test]
    fn a_corrupt_refusal_one_byte_under_the_output_violates_the_cap_law() {
        let v = violation(8, |cap| {
            if cap < PAYLOAD.len() {
                Err(Error::invalid_input(ORIGIN, CORRUPT))
            } else {
                honest(cap)
            }
        });
        assert!(v.detail().contains("not the over-cap refusal"), "{v}");
    }

    #[test]
    fn each_undocumented_refusal_violates_the_cap_law() {
        let other = || Error::invalid_input(ORIGIN, "some other refusal");
        let foreign = || Error::invalid_input("elsewhere", CORRUPT);
        let unsupported = || Error::unsupported(ORIGIN, CORRUPT);
        assert_ne!(unsupported().kind(), ErrorKind::InvalidInput);
        for refusal in [other, foreign, unsupported] {
            let v = violation(8, |_| Err(refusal()));
            assert!(v.detail().contains("undocumented refusal"), "{v}");
            // The same refusal one byte under the output is held to the same clause.
            let v = violation(8, |cap| {
                if cap < PAYLOAD.len() {
                    Err(refusal())
                } else {
                    honest(cap)
                }
            });
            assert!(v.detail().contains("undocumented refusal"), "{v}");
        }
    }
}
