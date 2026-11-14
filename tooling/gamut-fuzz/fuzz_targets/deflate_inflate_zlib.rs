//! fuzz · law — gamut-deflate's bounded-inflate law over arbitrary zlib bytes (#1144).
//!
//! This is the *same* function `gamut_deflate::invariants` exposes to the pinned-seed `proptest`
//! property in `crates/gamut-deflate/src/inflate.rs` (`tests::cap_law`),
//! `inflation_stays_within_cap`. The property samples this crate's own encoder output, intact or
//! with one byte flipped or its tail cut; this tier searches the whole byte space `inflate_zlib`
//! takes from PNG `IDAT`/`iCCP`/`zTXt`/`iTXt`, TIFF Deflate strips and DNG Deflate tiles.
//!
//! The input's **first two bytes** are the cap, a little-endian `u16`, and the rest is the zlib
//! stream, unaltered, so each committed seed is a cap followed by a real zlib stream. The `u16`
//! bounds every inflation here to 64 KiB less one byte, so an
//! allocation past libFuzzer's limit is a cap that did not hold, not a large legitimate stream.
//!
//! The law checks that an `Ok` never exceeds the cap, that the cap is exact (the same bytes under
//! a cap of their length, the over-cap refusal under one less), and that every `Err` is one of
//! the two documented refusals. Beyond that, the engine's own oracles apply: a panic or a hang in
//! `inflate_zlib` — or in `miniz_oxide` beneath it — is a defect.
//!
//! A crash found here is **minimised and promoted into a named deterministic test** in
//! `gamut-deflate`'s own suite. The corpus is a search aid, not the regression record.

#![no_main]

use gamut_deflate::invariants::inflation_stays_within_cap;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((cap, stream)) = data.split_first_chunk::<2>() else {
        return;
    };
    let max_out = usize::from(u16::from_le_bytes(*cap));
    if let Err(violation) = inflation_stays_within_cap(stream, max_out) {
        panic!("{violation}");
    }
});
