//! Prints the byte runs each DNG carries that its own structures do not account for — a vendor
//! preamble, interstitial filler, an appended trailer — with the segments bracketing each run and
//! a peek at its bytes.
//!
//! This is how a corpus file's `unaccounted_spans`/`unaccounted_bytes` expectations are derived,
//! and the first thing to run when byte accounting fails on a new file. Anything still in
//! `unclassified` after `deconstruct` is a **parser gap**, not a property of the file, so it is
//! reported separately and loudly. Triage only — asserts nothing.
//!
//! ```sh
//! cargo run --manifest-path tooling/gamut-dng-real-conformance/Cargo.toml \
//!     --example gaps -- third_party/gamut-dng-samples/leica/m10/f5381888.dng
//! ```

use gamut_dng::deconstruct;

fn main() {
    for path in std::env::args().skip(1) {
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        println!("{}", "=".repeat(78));
        println!("{path} ({} bytes)", data.len());
        let report = match deconstruct(&data) {
            Ok(r) => r,
            Err(e) => {
                println!("  deconstruct failed: {e}");
                continue;
            }
        };
        let segs = &report.segments;
        let spans = segs.unclaimed_spans();
        println!(
            "  {} segments | {} unaccounted run(s), {} bytes | classified={}",
            segs.segments.len(),
            spans.len(),
            segs.unclaimed_span_bytes(),
            segs.is_fully_classified(),
        );

        for span in &spans {
            let (start, end) = (span.range.start, span.range.end);
            let before = segs
                .segments
                .iter()
                .filter(|s| s.range.end <= start && s.range != span.range)
                .max_by_key(|s| s.range.end);
            let after = segs
                .segments
                .iter()
                .filter(|s| s.range.start >= end)
                .min_by_key(|s| s.range.start);
            println!("  {:?} [{start}..{end}) {} bytes", span.kind, end - start);
            println!("      preceded by: {:?}", before.map(|s| s.kind));
            println!("      followed by: {:?}", after.map(|s| s.kind));
            let peek_end = (end as usize).min(start as usize + 24);
            println!(
                "      bytes      : {:02x?}",
                &data[start as usize..peek_end]
            );
            if let Ok(text) = std::str::from_utf8(&data[start as usize..peek_end])
                && text.chars().all(|c| c.is_ascii_graphic() || c == '\0')
            {
                println!("      as ascii   : {text:?}");
            }
        }

        // Anything left here is a gamut parser gap: bytes nobody claimed and nobody named.
        if !segs.unclassified.is_empty() {
            println!(
                "  PARSER GAP: {} unclassified range(s), {} bytes: {:?}",
                segs.unclassified.len(),
                segs.unclassified_bytes(),
                segs.unclassified,
            );
        }
    }
}
