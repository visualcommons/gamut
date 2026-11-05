//! Exhaustive permutation coverage of the RFC 9649 §2.7 chunk-ordering rule.
//!
//! > All chunks necessary for reconstruction and color correction, that is, 'VP8X', 'ICCP', 'ANIM',
//! > 'ANMF', 'ALPH', 'VP8 ', and 'VP8L', MUST appear in the order described earlier. Readers SHOULD
//! > fail when chunks necessary for reconstruction and color correction are out of order.
//! >
//! > Metadata (Section 2.7.1.5) and unknown chunks (Section 2.7.1.6) MAY appear out of order.
//!
//! The unit tests in `src/webp.rs` check the adjacent inversions; this file walks **every**
//! permutation of the still-image reconstruction set and of a metadata/unknown set, so the rule is
//! pinned as a whole rather than at a few sampled points.

use gamut_riff::{FourCc, RiffWriter, Vp8xHeader, WebpLayout};

/// The still-image reconstruction sequence, in the order §2.7 mandates.
fn reconstruction_chunks() -> Vec<(FourCc, Vec<u8>)> {
    let vp8x = Vp8xHeader {
        alpha: true,
        icc_profile: true,
        canvas_width: 8,
        canvas_height: 8,
        ..Default::default()
    }
    .to_bytes()
    .expect("a valid canvas");
    vec![
        (FourCc::VP8X, vp8x.to_vec()),
        (FourCc::ICCP, b"icc".to_vec()),
        (FourCc::ALPH, vec![0x00, 0x11]),
        (FourCc::VP8L, vec![0x2f, 0x00]),
    ]
}

/// Assembles a file from `chunks` verbatim, bypassing the ordering the writers impose.
fn raw_file(chunks: &[(FourCc, Vec<u8>)]) -> Vec<u8> {
    let mut w = RiffWriter::new();
    for (fourcc, payload) in chunks {
        w.write_chunk(*fourcc, payload).expect("write");
    }
    w.finish().expect("finish")
}

/// Every permutation of `items`, by index (Heap's algorithm, iterative for clarity).
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    let mut out = Vec::new();
    let n = items.len();
    let mut indices: Vec<usize> = (0..n).collect();
    let mut counters = vec![0usize; n];
    out.push(indices.iter().map(|&i| items[i].clone()).collect());
    let mut i = 0;
    while i < n {
        if counters[i] < i {
            indices.swap(if i % 2 == 0 { 0 } else { counters[i] }, i);
            out.push(indices.iter().map(|&i| items[i].clone()).collect());
            counters[i] += 1;
            i = 0;
        } else {
            counters[i] = 0;
            i += 1;
        }
    }
    out
}

#[test]
fn exactly_the_spec_order_is_accepted() {
    // Of the 24 orderings of {VP8X, ICCP, ALPH, bitstream}, exactly one is the sequence §2.7
    // describes; every other must be refused. This is the whole rule in one assertion.
    let canonical = reconstruction_chunks();
    let mut accepted = Vec::new();
    for candidate in permutations(&canonical) {
        if WebpLayout::parse(&raw_file(&candidate)).is_ok() {
            accepted.push(
                candidate
                    .iter()
                    .map(|(f, _)| f.to_string())
                    .collect::<Vec<_>>(),
            );
        }
    }
    assert_eq!(
        accepted,
        vec![
            canonical
                .iter()
                .map(|(f, _)| f.to_string())
                .collect::<Vec<_>>()
        ],
        "exactly one of the 24 permutations conforms"
    );
}

#[test]
fn metadata_and_unknown_chunks_may_sit_anywhere() {
    // The exempt set, permuted against a fixed reconstruction prefix: every arrangement parses, and
    // every payload is still recovered.
    let odd = FourCc::from(*b"XYZW");
    let floating = vec![
        (FourCc::EXIF, b"exif".to_vec()),
        (FourCc::XMP, b"<x/>".to_vec()),
        (odd, b"private".to_vec()),
    ];
    let base = reconstruction_chunks();

    for arrangement in permutations(&floating) {
        // Interleave the exempt chunks at every insertion point of the reconstruction sequence.
        for split in 0..=base.len() {
            let mut chunks = base[..split].to_vec();
            chunks.extend(arrangement.iter().cloned());
            chunks.extend(base[split..].iter().cloned());

            // `WebpLayout` borrows the input, so the file has to outlive it.
            let file = raw_file(&chunks);
            let layout = WebpLayout::parse(&file)
                .unwrap_or_else(|e| panic!("split {split}, arrangement {arrangement:?}: {e}"));
            assert_eq!(layout.metadata.exif, Some(&b"exif"[..]));
            assert_eq!(layout.metadata.xmp, Some(&b"<x/>"[..]));
            assert_eq!(layout.unknown.len(), 1);
            assert_eq!(layout.unknown[0].payload, b"private");
        }
    }
}

#[test]
fn a_simple_file_needs_no_ordering_at_all() {
    // With one chunk there is nothing to order, so both bitstream kinds parse bare.
    for fourcc in [FourCc::VP8, FourCc::VP8L] {
        let file = raw_file(&[(fourcc, vec![0x2f, 0x00])]);
        assert!(WebpLayout::parse(&file).is_ok(), "{fourcc} alone");
    }
}

#[test]
fn repeated_chunks_do_not_count_as_a_regression() {
    // Equal ranks are not a regression — only a decrease is. A duplicated chunk in place is
    // tolerated (first wins), which keeps the rule about *order* rather than *multiplicity*.
    let mut chunks = reconstruction_chunks();
    chunks.insert(2, (FourCc::ICCP, b"second-icc".to_vec()));
    let file = raw_file(&chunks);
    let layout = WebpLayout::parse(&file).expect("a repeat in place is in order");
    assert_eq!(
        layout.metadata.icc,
        Some(&b"icc"[..]),
        "the first of the pair wins"
    );
}
