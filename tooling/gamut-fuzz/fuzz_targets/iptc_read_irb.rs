//! fuzz · robustness — `gamut_iptc::IptcReader::read_irb`, the entry point an embedded Photoshop
//! resource block hits.
//!
//! `docs/testing.md`'s per-crate table names `read_irb` as this crate's untrusted-input surface.
//! IPTC-IIM reaches a reader inside a Photoshop Image Resource Block — a JPEG `APP13`, a TIFF
//! `Photoshop` tag — and `read_irb` parses both layers: the `8BIM` resource stream, then the IIM
//! datasets of its `0x0404` resource, each length the writer chose (including the extended,
//! length-of-length form). The crate is `#![forbid(unsafe_code)]`, so a hostile block must end in a
//! typed error, never a panic, a hang, or an allocation past libFuzzer's malloc limit.
//!
//! Where a block parses, it is then merged into a `PhotoMetadata` view through
//! [`IptcReader::read`], which decodes every dataset value through the block's declared `1:90`
//! character set — the second half of the read a caller performs.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`. A crash is minimised
//! and promoted into a named deterministic case in `gamut-iptc`'s own suite.

#![no_main]

use gamut_iptc::IptcReader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let reader = IptcReader::new();
    if let Ok(Some(iim)) = reader.read_irb(data) {
        let _ = reader.read(Some(&iim), None);
    }
});
