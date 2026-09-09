//! Byte-accounting totality for [`gamut_png::deconstruct`] (issue #224): every PNG's segments
//! must tile `0..len` exactly, and the reported figures must match what the file actually holds.
//!
//! The fixtures come from **libpng**, not from gamut's encoder, wherever the claim is about
//! reading a foreign file: interlaced streams, forced filters and sub-byte depths are all things
//! `gamut_png::PngEncoder` cannot write, so a gamut-only corpus could not reach them, and a
//! filter histogram checked against gamut's own choice would be self-consistent rather than
//! correct.

mod common;

use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8, Rgba8};
use gamut_png::{
    ChunkStats, DeconstructLimits, FilterScan, FilterStrategy, FilterType, PngEncoder, Segment,
    SegmentKind, SkippedFilterScan, deconstruct, deconstruct_with_limits,
};

/// Folds over the segments asserting: non-empty, first starts at 0, each end chains to the next
/// start (contiguous, non-overlapping), and the last ends at `len` — the every-byte invariant.
/// Deliberately re-derived here rather than trusting `is_fully_classified`, which is the thing
/// under test.
fn assert_covers(segments: &[Segment], len: usize) {
    assert!(!segments.is_empty(), "at least one segment");
    assert_eq!(segments[0].range.start, 0, "coverage starts at 0");
    for pair in segments.windows(2) {
        assert_eq!(
            pair[0].range.end, pair[1].range.start,
            "segments are contiguous and non-overlapping"
        );
    }
    assert_eq!(
        segments.last().expect("non-empty").range.end,
        len,
        "coverage runs to end of file"
    );
    for s in segments {
        assert!(s.range.end > s.range.start, "no empty segment: {s:?}");
    }
}

/// A deterministic RGB pattern with enough local structure that filters differ between rows.
fn rgb(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            out.push((x ^ y) as u8);
            out.push(x.wrapping_mul(3).wrapping_add(y) as u8);
            out.push(x.wrapping_add(y.wrapping_mul(7)) as u8);
        }
    }
    out
}

fn encode_rgb(w: u32, h: u32) -> Vec<u8> {
    let src = rgb(w, h);
    let dims = Dimensions::new(w, h).expect("valid dimensions");
    let image = ImageRef::<Rgb8>::new(&src, dims).expect("buffer matches dimensions");
    let mut png = Vec::new();
    PngEncoder::new()
        .encode_image(image, &mut png)
        .expect("encode");
    png
}

#[test]
fn segments_tile_every_byte_of_a_gamut_encode() {
    for (w, h) in [(1, 1), (17, 13), (64, 40)] {
        let png = encode_rgb(w, h);
        let report = deconstruct(&png).expect("deconstruct");
        assert_covers(&report.segments, png.len());
        assert!(report.is_fully_classified(), "{report:?}");
        assert!(report.is_intact(), "{report:?}");
        assert_eq!(report.file_len, png.len());
    }
}

#[test]
fn segments_tile_every_byte_of_every_libpng_colour_type_and_depth() {
    for &(color_type, depth) in common::TABLE_12 {
        for interlace in [false, true] {
            let png = common::libpng_fixture(17, 13, color_type, depth, interlace);
            let report = deconstruct(&png).unwrap_or_else(|e| {
                panic!("deconstruct ct={color_type} depth={depth} interlace={interlace}: {e:?}")
            });
            assert_covers(&report.segments, png.len());
            assert!(
                report.is_intact(),
                "ct={color_type} depth={depth} interlace={interlace}: {report:?}"
            );
            assert_eq!(report.header.bit_depth, depth);
            assert_eq!(report.header.interlaced, interlace);
        }
    }
}

#[test]
fn chunk_totals_match_an_independent_scan() {
    let png = encode_rgb(40, 30);
    let report = deconstruct(&png).expect("deconstruct");

    // A naive second scan written here, so a defect in the walk's accumulation cannot agree with
    // itself. 8 signature bytes, then `length || type || data || crc`.
    let mut at = 8usize;
    let mut seen: Vec<([u8; 4], usize, usize)> = Vec::new();
    while at + 12 <= png.len() {
        let len = u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]) as usize;
        let ty = [png[at + 4], png[at + 5], png[at + 6], png[at + 7]];
        match seen.iter_mut().find(|(t, _, _)| *t == ty) {
            Some(entry) => {
                entry.1 += 1;
                entry.2 += len;
            }
            None => seen.push((ty, 1, len)),
        }
        at += 12 + len;
    }
    assert_eq!(at, png.len(), "the naive scan must consume the file too");

    let got: Vec<_> = report
        .chunks
        .iter()
        .map(|c| (c.chunk_type, c.count, c.payload_bytes))
        .collect();
    assert_eq!(got, seen, "chunk table, in first-appearance order");

    // Signature + every chunk's payload and framing is the whole file.
    let total: usize = report.chunks.iter().map(ChunkStats::total_bytes).sum();
    assert_eq!(total + 8, png.len());
    assert_eq!(report.framing_bytes(), report.chunks.len() * 12);
}

/// A chunk type that appears more than once must accumulate, not overwrite.
///
/// Every other fixture here carries at most one chunk of each type, so the accumulate arm of the
/// chunk table never ran: `count` stayed at the 1 it is inserted with and `payload_bytes` at the
/// first chunk's length, and no assertion could tell.
#[test]
fn repeated_chunk_types_accumulate_count_and_payload() {
    let first: &[u8] = b"Author\0alice";
    let second: &[u8] = b"Comment\0a considerably longer comment";
    let png = common::png_from_chunks(&[
        common::chunk(b"IHDR", &common::ihdr_payload(4, 4, 8, 2, 0)),
        common::chunk(b"tEXt", first),
        common::chunk(b"tEXt", second),
        common::chunk(b"IDAT", &common::zlib(&[0u8; 4 * (4 * 3 + 1)])),
        common::chunk(b"IEND", &[]),
    ]);

    let report = deconstruct(&png).expect("deconstruct");
    assert_covers(&report.segments, png.len());

    let text = report.chunk(b"tEXt").expect("tEXt accounted");
    assert_eq!(text.count, 2, "both chunks counted");
    assert_eq!(
        text.payload_bytes,
        first.len() + second.len(),
        "payloads summed, not overwritten"
    );
    assert_eq!(text.framing_bytes(), 24, "12 framing bytes per chunk");
    assert_eq!(text.total_bytes(), first.len() + second.len() + 24);
    // The table lists each type once, in first-appearance order.
    assert_eq!(
        report
            .chunks
            .iter()
            .map(|c| c.chunk_type)
            .collect::<Vec<_>>(),
        vec![*b"IHDR", *b"tEXt", *b"IDAT", *b"IEND"]
    );
}

/// A stream large enough to split across several IDAT chunks: the same accumulation, on the path
/// that actually produces it in production rather than a hand-built file.
/// A synthetic chunk type for the quadratic regression fixture below: four lowercase letters, so
/// it is ancillary, private, and can never collide with `IHDR`, `IDAT` or `IEND`. 26⁴ = 456 976
/// distinct types, comfortably more than the fixture uses.
fn synthetic_type(i: usize) -> [u8; 4] {
    [
        b'a' + (i % 26) as u8,
        b'a' + (i / 26 % 26) as u8,
        b'a' + (i / 676 % 26) as u8,
        b'a' + (i / 17_576 % 26) as u8,
    ]
}

/// A file whose every chunk type is distinct is tallied one entry per type, in first-appearance
/// order, against the same bytes carrying one type throughout.
///
/// A chunk type is four unvalidated bytes and the walk never drops a chunk, so an attacker
/// chooses how many *distinct* types a file carries — one per 12-byte chunk, if they like.
/// Accumulating the per-type totals with a linear scan made this quadratic in the file length
/// (measured: 4.8 MB → 40.9 s), reachable from `gamut inspect` on an untrusted file.
///
/// The complexity claim itself is pinned inside the crate, where the tally's lookup work can be
/// counted (`deconstruct::tests::the_tally_probes_once_per_chunk_whatever_the_number_of_distinct_types`):
/// a wall-clock ratio between two runs in the blocking gate is flaky under `llvm-cov` and parallel
/// test binaries, and timing belongs to `benches/`. What this test adds from the public side is
/// the *content* — one `ChunkStats` per distinct type, in order, and one entry counting every
/// chunk when the type repeats — which is what the index exists to produce.
///
/// **Why 1024**, where this once built 262 144. The content claim is per entry and holds at any
/// count past a handful; what the count must clear is `synthetic_type`'s own arithmetic, whose
/// digits roll over at 26 and 676, so 1024 varies three of the four type bytes and still carries
/// two orders of magnitude more distinct types than any real PNG. The larger figure pinned
/// nothing further — it did not fail under the quadratic defect either, it merely took about
/// 17 s — while costing ~3.1 MB of fixture per half on every `mise run test`, every coverage run,
/// and once per mutant in every `gamut-png` mutation shard.
#[test]
fn every_distinct_chunk_type_gets_its_own_tally_entry() {
    /// Empty chunks between IHDR and IEND: 12 bytes each, so ~12 KB per half.
    const CHUNKS: usize = 1024;

    let build = |distinct: bool| {
        let mut framed = Vec::with_capacity(CHUNKS + 2);
        framed.push(common::chunk(b"IHDR", &common::ihdr_payload(1, 1, 8, 2, 0)));
        framed.extend(
            (0..CHUNKS).map(|i| common::chunk(&synthetic_type(if distinct { i } else { 0 }), &[])),
        );
        framed.push(common::chunk(b"IEND", &[]));
        common::png_from_chunks(&framed)
    };
    let repeated = build(false);
    let distinct = build(true);
    assert_eq!(
        repeated.len(),
        distinct.len(),
        "the two halves are the same bytes apart from the types they use"
    );

    let repeated_report = deconstruct(&repeated).expect("deconstruct");
    let distinct_report = deconstruct(&distinct).expect("deconstruct");

    assert_eq!(
        distinct_report.chunks.len(),
        CHUNKS + 2,
        "IHDR, one entry per distinct type, IEND"
    );
    assert!(
        distinct_report.chunks.iter().all(|stats| stats.count == 1),
        "every synthetic type appears exactly once"
    );
    assert_eq!(
        repeated_report.chunks.len(),
        3,
        "IHDR, the one repeated type, IEND"
    );
    assert_eq!(repeated_report.chunks[1].count, CHUNKS);
}

#[test]
fn a_multi_idat_encode_accumulates_every_idat() {
    // Incompressible, so the zlib stream stays far above the 64 KiB per-chunk cap.
    let (w, h) = (256u32, 256u32);
    let src = common::corpus::noise_rgb(w);
    let dims = Dimensions::new(w, h).expect("valid dimensions");
    let image = ImageRef::<Rgb8>::new(&src, dims).expect("buffer matches dimensions");
    let mut png = Vec::new();
    PngEncoder::new()
        .encode_image(image, &mut png)
        .expect("encode");

    let report = deconstruct(&png).expect("deconstruct");
    assert_covers(&report.segments, png.len());
    let idat = report.chunk(b"IDAT").expect("IDAT accounted");
    assert!(idat.count > 1, "the fixture must actually split: {idat:?}");
    assert_eq!(
        idat.payload_bytes, report.idat_compressed,
        "the chunk table and the compressed total are the same bytes counted twice"
    );
    assert!(report.is_intact(), "{report:?}");
}

#[test]
fn trailing_bytes_after_iend_are_a_trailer() {
    let mut png = encode_rgb(8, 8);
    let clean = png.len();
    png.extend_from_slice(b"junk after the datastream");
    let report = deconstruct(&png).expect("deconstruct");

    assert_covers(&report.segments, png.len());
    let last = report.segments.last().expect("non-empty");
    assert_eq!(last.kind, SegmentKind::Trailer);
    assert_eq!(last.range, clean..png.len());
    // A trailer is not damage the walk failed to classify, but the file is not pristine.
    assert!(report.is_fully_classified());
    assert!(!report.is_intact());
}

#[test]
fn a_truncated_tail_is_reported_not_an_error() {
    let full = encode_rgb(24, 24);
    // Cut inside the IDAT payload: the chunk header frames, but its data overruns the input.
    let png = &full[..full.len() - 20];
    let report = deconstruct(png).expect("a truncated file still has a header to report on");

    assert_covers(&report.segments, png.len());
    assert_eq!(
        report.segments.last().expect("non-empty").kind,
        SegmentKind::Truncated
    );
    assert!(report.is_fully_classified());
    assert!(!report.is_intact(), "truncation is not intact");
    // Everything derived from IHDR survives the damage — that is the point of the split.
    assert_eq!(report.header.width, 24);
    assert!(report.filtered_len > 0);
}

#[test]
fn unknown_ancillary_and_critical_chunks_are_accounted() {
    let extra: [([u8; 4], &[u8]); 2] = [(*b"abCd", &[1, 2, 3]), (*b"ABCD", &[4])];
    let png = common::libpng_with_extra_chunks(12, 9, &extra);
    let report = deconstruct(&png).expect("an unknown critical chunk is reported, not an error");

    assert_covers(&report.segments, png.len());
    let ancillary = report.chunk(b"abCd").expect("unknown ancillary accounted");
    let critical = report.chunk(b"ABCD").expect("unknown critical accounted");
    assert_eq!(ancillary.payload_bytes, 3);
    assert_eq!(critical.payload_bytes, 1);
    assert!(
        ancillary.is_ancillary(),
        "lowercase first byte is ancillary"
    );
    assert!(!critical.is_ancillary(), "uppercase first byte is critical");
}

#[test]
fn a_crc_mismatch_is_flagged_not_fatal() {
    let mut png = encode_rgb(8, 8);
    // Corrupt IEND's stored CRC, not any payload: every chunk still frames and IHDR still parses,
    // so the only thing wrong with the file is a checksum. Corrupting a payload instead would
    // make IHDR unparsable, which is a hard error and a different claim.
    let last = png.len() - 1;
    png[last] ^= 0xFF;
    let report = deconstruct(&png).expect("a CRC mismatch is reported, not an error");

    assert_covers(&report.segments, png.len());
    let bad = report
        .segments
        .iter()
        .filter_map(|s| match s.kind {
            SegmentKind::Chunk {
                chunk_type,
                crc_ok: false,
                ..
            } => Some(chunk_type),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(bad, vec![*b"IEND"], "exactly the damaged chunk is flagged");
    assert!(report.is_fully_classified());
    assert!(!report.is_intact(), "a bad CRC is not intact");
}

#[test]
fn the_filter_histogram_matches_the_filter_libpng_was_forced_to_use() {
    // libpng, not gamut, picks the filters here, so this cannot be satisfied by a self-consistent
    // round trip: it is the differential half of the report's claim.
    let forced = [
        (libpng_oracle::FILTER_NONE, FilterType::None),
        (libpng_oracle::FILTER_SUB, FilterType::Sub),
        (libpng_oracle::FILTER_UP, FilterType::Up),
        (libpng_oracle::FILTER_AVG, FilterType::Average),
        (libpng_oracle::FILTER_PAETH, FilterType::Paeth),
    ];
    for (mask, expected) in &forced {
        let png = common::libpng_forced_filter(20, 14, *mask);
        let report = deconstruct(&png).expect("deconstruct");
        let filters = report
            .filters
            .histogram()
            .expect("a sound IDAT stream yields a histogram");

        assert_eq!(filters.total(), 14, "one filter byte per scanline");
        assert_eq!(
            filters.count(*expected),
            14,
            "every row used {expected:?} (mask {mask:#04x})"
        );
    }
}

/// The histogram must advance one scanline at a time.
///
/// Every other histogram assertion here forces a single filter for the whole image, which cannot
/// tell a correct per-row walk from one that re-reads the same byte: both report `height` of the
/// one filter. This fixture's rows choose differently, so a stalled cursor collapses the
/// distribution to a single bucket and is visible.
#[test]
fn the_histogram_walks_each_scanline_not_the_first_one_repeatedly() {
    const SIDE: u32 = 64;
    let src = common::corpus::sprite_rgba(SIDE);
    let dims = Dimensions::new(SIDE, SIDE).expect("valid dimensions");
    let image = ImageRef::<Rgba8>::new(&src, dims).expect("buffer matches dimensions");
    let mut png = Vec::new();
    PngEncoder::new()
        .with_filter(FilterStrategy::MinSumAbs)
        .encode_image(image, &mut png)
        .expect("encode");

    let report = deconstruct(&png).expect("deconstruct");
    let h = report.filters.histogram().expect("sound stream");
    assert_eq!(h.total(), SIDE, "one filter byte per scanline");

    let used = [
        FilterType::None,
        FilterType::Sub,
        FilterType::Up,
        FilterType::Average,
        FilterType::Paeth,
    ]
    .into_iter()
    .filter(|&f| h.count(f) > 0)
    .count();
    assert!(
        used >= 2,
        "this fixture's rows must not all choose the same filter, got {used} distinct"
    );
}

#[test]
fn interlaced_filtered_length_is_the_per_pass_sum() {
    // 5x3 and 1x1 leave several Adam7 passes empty; an empty pass contributes no bytes at all,
    // not even a filter byte (§7.3).
    for (w, h) in [(1, 1), (5, 3), (17, 13)] {
        let png = common::libpng_fixture(w, h, libpng_oracle::COLOR_RGB, 8, true);
        let report = deconstruct(&png).expect("deconstruct");

        let summed: usize = report.passes.iter().map(|p| p.filtered_len).sum();
        assert_eq!(
            summed, report.filtered_len,
            "{w}x{h}: passes sum to the whole"
        );
        assert!(
            report.passes.iter().all(|p| p.width > 0 && p.height > 0),
            "empty passes are omitted, not zero-sized: {:?}",
            report.passes
        );

        let rows: u32 = report.passes.iter().map(|p| p.height).sum();
        assert_eq!(
            report.filters.histogram().expect("sound stream").total(),
            rows,
            "{w}x{h}: one filter byte per scanline of every non-empty pass"
        );
    }
}

#[test]
fn sub_byte_row_padding_is_counted() {
    // 5 pixels at depth 4 is 20 bits, which pads to 3 bytes per row -- `div_ceil`, not `/`.
    let png = common::libpng_fixture(5, 3, libpng_oracle::COLOR_GRAY, 4, false);
    let report = deconstruct(&png).expect("deconstruct");

    assert_eq!(report.passes.len(), 1, "not interlaced");
    assert_eq!(report.passes[0].row_bytes, 3);
    assert_eq!(report.filtered_len, 3 * (3 + 1));
}

#[test]
fn a_corrupt_zlib_stream_with_a_valid_crc_yields_no_histogram() {
    // The only fixture that falsifies `is_intact`'s `filters.is_some()` conjunct on its own:
    // framing is perfect, every CRC is valid, and only the compressed payload is nonsense.
    let png = common::png_with_garbage_idat(16, 8);
    let report = deconstruct(&png).expect("a corrupt IDAT is reported, not an error");

    assert_covers(&report.segments, png.len());
    assert!(report.is_fully_classified());
    assert!(
        report.segments.iter().all(|s| match s.kind {
            SegmentKind::Chunk { crc_ok, .. } => crc_ok,
            _ => true,
        }),
        "every CRC is valid in this fixture"
    );
    assert_eq!(
        report.filters,
        FilterScan::Skipped(SkippedFilterScan::CorruptStream),
        "the scan is the only casualty, and it names why"
    );
    assert!(
        report.filters.is_damage(),
        "a corrupt payload is damage, not a budget refusal"
    );
    assert!(!report.is_intact());
    // Framing- and IHDR-derived figures are unaffected.
    assert_eq!(report.header.width, 16);
    assert!(report.idat_compressed > 0);
    assert!(report.filtered_len > 0);
}

#[test]
fn an_undefined_filter_code_is_named_and_is_damage() {
    // The fourth skip reason, and the only one with no fixture of its own: a stream that inflates
    // to exactly the right length but whose scanline carries a filter code PNG SS9.1 does not
    // define. Without this the `FilterType::from_code` guard can be deleted -- counting an
    // undefined code as `None` and reporting a bogus histogram for a hostile file -- and every
    // other assertion in the suite still passes.
    let filtered = [9u8, 0, 0, 0, 0, 0, 0]; // 2x1 RGB8: one row, 6 bytes, filter code 9.
    let png = common::png_from_chunks(&[
        common::chunk(b"IHDR", &common::ihdr_payload(2, 1, 8, 2, 0)),
        common::chunk(b"IDAT", &common::zlib(&filtered)),
        common::chunk(b"IEND", &[]),
    ]);
    let report = deconstruct(&png).expect("an undefined filter code is reported, not an error");

    assert_covers(&report.segments, png.len());
    assert_eq!(
        report.filtered_len, 7,
        "one row of 6 bytes plus its filter byte"
    );
    assert_eq!(
        report.filters,
        FilterScan::Skipped(SkippedFilterScan::UndefinedFilterCode),
        "the scan names the undefined code rather than any other reason"
    );
    assert!(
        report.filters.is_damage(),
        "an undefined filter code is a statement about the bytes"
    );
    assert!(!report.is_intact());
}

#[test]
fn an_unread_file_is_intact_but_not_verified() {
    // The distinction `is_verified` exists to make. Nothing is known to be wrong with an
    // over-budget file, so `is_intact` is true -- but its IDAT was never inflated, so no claim
    // about the compressed data has been checked and `is_verified` is false. Collapsing the two
    // is what let an archival gate pass a file it never read.
    let png = common::png_with_huge_ihdr();
    let report = deconstruct(&png).expect("deconstruct");

    assert_eq!(
        report.filters,
        FilterScan::Skipped(SkippedFilterScan::OverBudget)
    );
    assert!(
        !report.filters.is_damage(),
        "a budget refusal is not damage"
    );
    assert!(
        !report.filters.is_counted(),
        "and it is not a reading either"
    );
    assert!(report.is_intact(), "nothing is known against this file");
    assert!(
        !report.is_verified(),
        "but nothing about its compressed data was checked"
    );
}

#[test]
fn the_chunk_ceiling_admits_exactly_its_own_count_and_refuses_one_more() {
    // The chunk count is chosen by the input -- a chunk costs 12 bytes and buys a segment -- so
    // the walk caps it. Asserted *at the boundary* rather than far past it: a file well over the
    // ceiling is refused by `>`, `>=` and `==` alike, so only the exact count separates them.
    // Ten chunks here — IHDR, eight fillers and IEND — under eleven segments, because the
    // signature is a segment but not a chunk: `max_chunks` counts what its name says, so a
    // ceiling of ten admits this file and a ceiling of nine refuses it.
    const CHUNKS: usize = 10;
    let mut chunks = vec![common::chunk(b"IHDR", &common::ihdr_payload(1, 1, 8, 0, 0))];
    for _ in 0..8 {
        chunks.push(common::chunk(b"crUD", &[]));
    }
    chunks.push(common::chunk(b"IEND", &[]));
    let png = common::png_from_chunks(&chunks);

    let exact = DeconstructLimits::default().with_max_chunks(CHUNKS);
    let report = deconstruct_with_limits(&png, exact)
        .expect("a file of exactly the ceiling's size is admitted, not refused");
    assert_eq!(
        report.segments.len(),
        CHUNKS + 1,
        "the signature segment is not a chunk"
    );
    assert!(report.is_fully_classified(), "and it reports normally");

    let one_short = DeconstructLimits::default().with_max_chunks(CHUNKS - 1);
    let err = deconstruct_with_limits(&png, one_short)
        .expect_err("one past the ceiling the walk refuses rather than allocating");
    assert!(
        err.to_string().contains("more chunks"),
        "the error names the ceiling it hit, got: {err}"
    );

    // IHDR is a chunk and is counted like one. The walk pushes it before the loop that reads the
    // rest, so a ceiling checked only inside that loop let it through and `with_max_chunks(N)`
    // meant N + 1 in that one respect — visibly so at zero, which admitted a whole file.
    let ihdr_only = common::png_from_chunks(&chunks[..1]);
    let report =
        deconstruct_with_limits(&ihdr_only, DeconstructLimits::default().with_max_chunks(1))
            .expect("one chunk under a ceiling of one");
    assert_eq!(report.chunks.len(), 1, "IHDR alone, and it is a chunk");
    let err = deconstruct_with_limits(&ihdr_only, DeconstructLimits::default().with_max_chunks(0))
        .expect_err("no chunk at all fits a ceiling of zero");
    assert!(
        err.to_string().contains("more chunks"),
        "the error names the ceiling it hit, got: {err}"
    );
}

#[test]
fn the_image_budget_is_the_callers_to_set() {
    // `with_max_image_bytes` has to be observable, or the walk silently keeps the decoder's
    // default and `deconstruct_with_limits` is `deconstruct` with extra steps. A one-byte budget
    // turns an ordinary small file -- comfortably scanned under the default -- into a refusal.
    let png = common::minimal_png();
    assert!(
        deconstruct(&png).expect("deconstruct").filters.is_counted(),
        "the fixture is scanned under the default budget"
    );

    let stingy = DeconstructLimits::default().with_max_image_bytes(1);
    let report = deconstruct_with_limits(&png, stingy).expect("a budget refusal is not an error");
    assert_eq!(
        report.filters,
        FilterScan::Skipped(SkippedFilterScan::OverBudget),
        "the caller's budget decides, not the decoder's default"
    );
}

#[test]
fn a_sound_file_is_both_read_and_verified() {
    // The positive side of `is_counted` and `is_verified`. Without it both can be pinned to
    // `false` by the negative cases alone -- an over-budget file satisfies every assertion they
    // make -- and the verdict a gate depends on would be one that always says no.
    let png = common::minimal_png();
    let report = deconstruct(&png).expect("deconstruct");

    assert!(
        report.filters.is_counted(),
        "a sound stream is read, not skipped"
    );
    assert!(report.filters.histogram().is_some(), "so it has counts");
    assert!(report.is_intact(), "and nothing is held against it");
    assert!(
        report.is_verified(),
        "which together with having been read is what verification means"
    );
}

#[test]
fn an_over_budget_image_reports_everything_but_the_histogram() {
    // A hand-built IHDR claiming 2^30 x 2^30 with a tiny IDAT: the image it implies is far past
    // the decoder's byte budget, so the walk must decline to inflate rather than try. Without
    // this the budget comparison is never exercised.
    let png = common::png_with_huge_ihdr();
    let report = deconstruct(&png).expect("an oversized header is reported, not an error");

    assert_covers(&report.segments, png.len());
    assert_eq!(
        report.filters,
        FilterScan::Skipped(SkippedFilterScan::OverBudget),
        "declined: over the decoder's byte budget"
    );
    assert!(
        report.native_bytes().expect("representable") > (64 << 20),
        "the implied image is huge"
    );
    assert_eq!(report.header.width, 1 << 30);
    // And so this file is *not* reported as damaged: nothing here can tell whether its IDAT is
    // sound, and no decoder in the workspace could read it either, so claiming damage would be
    // claiming knowledge the walk does not have.
    assert!(!report.filters.is_damage());
    assert!(report.is_intact(), "{report:?}");
}

/// An image exactly at the decoder's byte budget must still be scanned.
///
/// 4096x4096 RGBA8 is 67 108 864 native bytes — the default budget to the byte — but 67 112 960
/// *filtered*, one more per scanline. A budget stated over the filtered stream therefore declined
/// it, and `is_intact` reported an image the decoder decodes as damaged. Cheap despite the
/// dimensions: nothing allocates `filtered_len`, and the 16-byte IDAT stops the scan at the
/// length check, so the reason is `LengthMismatch` — the file was scanned — and never
/// `OverBudget`.
#[test]
fn an_image_at_the_decoders_byte_budget_is_still_scanned() {
    let png = common::png_from_chunks(&[
        common::chunk(b"IHDR", &common::ihdr_payload(4096, 4096, 8, 6, 0)),
        common::chunk(b"IDAT", &common::zlib(&[0u8; 16])),
        common::chunk(b"IEND", &[]),
    ]);
    let report = deconstruct(&png).expect("deconstruct");

    assert_eq!(
        report.native_bytes(),
        Some(64 << 20),
        "exactly the decoder's default budget"
    );
    assert!(
        report.filtered_len > 64 << 20,
        "and past it once the filter bytes are counted: {}",
        report.filtered_len
    );
    assert_eq!(
        report.filters,
        FilterScan::Skipped(SkippedFilterScan::LengthMismatch),
        "scanned, and stopped by this file's short stream — not declined for budget"
    );
}

/// A header whose filtered stream overflows `usize` still reports, and its ratio is finite.
///
/// §11.2.1 allows dimensions up to 2³¹−1 each, so 2³¹−1 square at RGBA16 implies 2⁶⁵ filtered
/// bytes: `filtered_len` saturates to 0 rather than wrapping, and `idat_ratio` would otherwise
/// divide by it. Thirteen header bytes reach this, and `gamut inspect` prints the ratio for every
/// file it reads, so the guard in `idat_ratio` is live code on a hostile-input path — not the dead
/// branch a filtered-stream budget would have made it.
#[test]
fn a_header_whose_stream_overflows_reports_a_zero_ratio_rather_than_dividing_by_it() {
    let png = common::png_from_chunks(&[
        common::chunk(
            b"IHDR",
            &common::ihdr_payload(0x7FFF_FFFF, 0x7FFF_FFFF, 16, 6, 0),
        ),
        common::chunk(b"IDAT", &common::zlib(&[0u8; 8])),
        common::chunk(b"IEND", &[]),
    ]);
    let report = deconstruct(&png).expect("an unrepresentable stream is reported, not an error");

    assert_covers(&report.segments, png.len());
    assert_eq!(
        report.filtered_len, 0,
        "the implied stream is not representable"
    );
    assert!(report.idat_compressed > 0, "there is a numerator to divide");
    assert_eq!(
        report.idat_ratio(),
        0.0,
        "no division by zero, and not an infinity"
    );
    assert!(
        report.passes.is_empty(),
        "no pass geometry is representable either"
    );
}

/// The interlaced twin of the case above, which is where the two overflow checks can disagree.
///
/// Adam7 splits the image into seven smaller passes, so a header can be unrepresentable overall
/// while every individual pass fits `usize`. `adam7::expected_stream_len` fails on the seven-pass
/// *sum*, so `filtered_len` saturates to 0; `pass_stats` has to fail on the same sum or the report
/// contradicts itself -- seven passes described, and a `filtered_len` of 0 that `idat_ratio` then
/// reports as `0.0%` as though it were a measurement.
#[test]
fn an_interlaced_header_whose_passes_fit_but_whose_sum_does_not_reports_no_geometry() {
    let png = common::png_from_chunks(&[
        common::chunk(
            b"IHDR",
            &common::ihdr_payload(0x7FFF_FFFF, 0x7FFF_FFFF, 16, 6, 1),
        ),
        common::chunk(b"IDAT", &common::zlib(&[0u8; 8])),
        common::chunk(b"IEND", &[]),
    ]);
    let report = deconstruct(&png).expect("an unrepresentable stream is reported, not an error");

    assert_covers(&report.segments, png.len());
    assert_eq!(
        report.filtered_len, 0,
        "the seven-pass sum is not representable"
    );
    assert!(
        report.passes.is_empty(),
        "and the per-pass geometry must saturate with it, not describe seven passes \
         against a zero total"
    );
    assert_eq!(report.idat_ratio(), 0.0, "no division by zero");
}

#[test]
fn a_file_with_no_header_to_report_on_is_an_error() {
    assert!(deconstruct(&[]).is_err(), "empty input");
    assert!(deconstruct(b"not a png at all").is_err(), "bad signature");

    let signature_only = common::SIGNATURE.to_vec();
    assert!(deconstruct(&signature_only).is_err(), "no chunk at all");

    let mut first_not_ihdr = common::SIGNATURE.to_vec();
    first_not_ihdr.extend_from_slice(&common::chunk(b"gAMA", &45455u32.to_be_bytes()));
    assert!(
        deconstruct(&first_not_ihdr).is_err(),
        "first chunk is not IHDR"
    );

    let mut bad_ihdr = common::SIGNATURE.to_vec();
    bad_ihdr.extend_from_slice(&common::chunk(b"IHDR", &[0u8; 13]));
    assert!(deconstruct(&bad_ihdr).is_err(), "zero dimensions in IHDR");
}

#[test]
fn the_derived_ratios_are_the_stated_quotients() {
    let png = encode_rgb(32, 24);
    let report = deconstruct(&png).expect("deconstruct");

    let pixels = f64::from(report.header.width) * f64::from(report.header.height);
    assert!(
        (report.bits_per_pixel() - (report.file_len as f64 * 8.0 / pixels)).abs() < 1e-9,
        "bits_per_pixel is the whole file over the pixel count"
    );
    assert!(
        (report.idat_ratio() - (report.idat_compressed as f64 / report.filtered_len as f64)).abs()
            < 1e-9,
        "idat_ratio is IDAT over the filtered stream"
    );
    assert_eq!(
        report.overhead_bytes(),
        report.file_len - report.idat_compressed
    );
    // A real photo-ish pattern must actually compress, or the fixture is not measuring anything.
    assert!(report.idat_ratio() < 1.0, "{}", report.idat_ratio());
}

#[test]
fn a_brute_force_encode_still_accounts_and_reports_its_filters() {
    // The strategy that costs the most and is most likely to trip an accounting assumption.
    let src = rgb(48, 32);
    let dims = Dimensions::new(48, 32).expect("valid dimensions");
    let image = ImageRef::<Rgb8>::new(&src, dims).expect("buffer matches dimensions");
    let mut png = Vec::new();
    PngEncoder::new()
        .with_filter(FilterStrategy::BruteForce)
        .encode_image(image, &mut png)
        .expect("encode");

    let report = deconstruct(&png).expect("deconstruct");
    assert_covers(&report.segments, png.len());
    assert!(report.is_intact());
    assert_eq!(
        report.filters.histogram().expect("sound stream").total(),
        32
    );
}
