//! The conformance badge's claim, asserted: the sandbox's whole profile battery renders within
//! `gamut-cmm`'s TIGHT bound of Little-CMS at every pair, intent and BPC setting the panels
//! offer. Placed under `tests/` only because the badge module exists under the `oracle` feature.

#![cfg(feature = "oracle")]

use gamut_icc_sandbox::conformance::{self, BOUND, PAIRS};
use gamut_icc_sandbox::profiles::Battery;

#[test]
fn battery_matches_lcms2_within_the_tight_bound() {
    let battery = Battery::build().expect("battery builds");
    let report = conformance::run(&battery).expect("every pair links in both engines");
    // 7 pairs × (4 intents × 2 BPC settings − absolute+BPC).
    assert_eq!(report.cells.len(), PAIRS.len() * 7);
    let worst = report.worst().expect("cells ran");
    assert!(
        report.passes(),
        "max ΔE00 {:.3e} ≥ {BOUND:e} at {:?} → {:?}, {:?}, BPC {}",
        worst.max_de00,
        worst.src,
        worst.dst,
        worst.intent,
        worst.bpc
    );
}
