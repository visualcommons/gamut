//! fuzz · robustness — `gamut_heic`'s typed `hvcC` record and NAL layer, the second half of the
//! crate's untrusted-input surface (`docs/testing.md`'s table names container `parse` **and** NAL
//! `parse`).
//!
//! An `hvcC` record is a length-prefixed array-of-arrays whose counts and lengths all come from
//! the file, and the item payload behind it is a chain of `nal_length_size`-byte length prefixes.
//! Both are `#![forbid(unsafe_code)]` offset arithmetic, so a hostile record must end in a typed
//! error rather than a panic or a spin.
//!
//! The check beyond the crash oracle is the **composition the API documents**:
//! `annex_b` is defined as `annex_b_parameter_sets` followed by `annex_b_payload`, appended to
//! the caller's buffer. Two callers rely on that split — an Annex-B decoder takes the whole
//! stream, an Android MediaCodec-shaped API takes `csd-0` and the samples separately — so the
//! halves drifting from the whole is a real defect that produces no crash at all. The target
//! asserts byte equality *and* that the two forms agree on success, including the documented
//! "bytes already appended are left in place" behaviour on error.
//!
//! `validate_still_payload` is driven for its own sake: it re-walks the payload through
//! `NalHeader::parse`, a different reach from the Annex-B emitters.
//!
//! ## Input framing
//!
//! `u16` big-endian record length, the `hvcC` record, then the item payload. A two-byte length
//! rather than a byte lets the engine reach records past 255 bytes (a real record with several
//! parameter sets is bigger), and taking it from the front means a mutation inside the record
//! does not also reframe the payload.
//!
//! A crash found here is **minimised and promoted into a named deterministic case** in
//! `gamut-heic`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_heic::{HevcConfig, NalHeader, iter_nal_units};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((length, rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let split = usize::from(u16::from_be_bytes(*length)).min(rest.len());
    let (record, payload) = rest.split_at(split);

    let Ok(config) = HevcConfig::parse(record) else {
        return;
    };

    // The documented composition: `annex_b` is the two halves, in order, appended to `out`.
    let mut whole = Vec::new();
    let whole_result = config.annex_b(payload, &mut whole);
    let mut halves = Vec::new();
    config.annex_b_parameter_sets(&mut halves);
    let halves_result = config.annex_b_payload(payload, &mut halves);
    assert_eq!(
        whole_result.is_ok(),
        halves_result.is_ok(),
        "annex_b and its two halves disagree on success"
    );
    assert_eq!(whole, halves, "annex_b is not its two halves concatenated");

    // Appending, not replacing: a caller reusing a scratch buffer keeps what was there.
    let mut reused = vec![0xEE; 3];
    let _ = config.annex_b(payload, &mut reused);
    assert_eq!(&reused[..3], &[0xEE; 3], "annex_b overwrote the buffer");
    assert_eq!(
        &reused[3..],
        &whole[..],
        "annex_b appended something different to a non-empty buffer"
    );

    // The NAL layer on its own reach: the still-image constraint re-walks the payload through
    // `NalHeader::parse`.
    let _ = config.validate_still_payload(payload);
    for nal in iter_nal_units(payload, config.nal_length_size()) {
        let Ok(nal) = nal else { break };
        assert!(!nal.is_empty(), "iter_nal_units yielded an empty NAL unit");
        let _ = NalHeader::parse(nal);
    }
});
