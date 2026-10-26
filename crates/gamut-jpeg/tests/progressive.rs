//! Integration tests for the progressive (SOF2) encoder, exercising only the public API.
//!
//! The workhorse is a **coefficient-exactness** gate: because Huffman coding is lossless and the
//! progressive scan script delivers every coefficient bit down to `Al = 0`, a progressive stream and
//! the baseline stream of the same input carry identical quantized coefficients — so decoding both
//! through gamut's own decoder must produce byte-identical images. Any deviation is a bug. A stream
//! walker then checks the emitted structure (scan script, per-scan spectral fields, per-scan DHTs,
//! restart cadence) and that the EOB-run machinery is actually exercised.

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_jpeg::{
    ChromaSubsampling, JpegColorMode, JpegDecoder, JpegEncoder, JpegProcess, ProgressiveScript,
};

/// A deterministic per-pixel-distinct pattern (varies on both axes) so every coordinate is
/// load-bearing — a mis-indexed scan diverges somewhere.
fn pattern(i: usize) -> u8 {
    ((i * 31 + 17) % 251) as u8
}

/// The colour configuration under test.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Gray,
    C444,
    C422,
    C420,
}

impl Mode {
    const ALL: [Mode; 4] = [Mode::Gray, Mode::C444, Mode::C422, Mode::C420];

    fn channels(self) -> u32 {
        match self {
            Mode::Gray => 1,
            _ => 3,
        }
    }

    fn subsampling(self) -> ChromaSubsampling {
        match self {
            Mode::Gray | Mode::C444 => ChromaSubsampling::Ycbcr444,
            Mode::C422 => ChromaSubsampling::Ycbcr422,
            Mode::C420 => ChromaSubsampling::Ycbcr420,
        }
    }
}

/// Encodes `src` with gamut in the given mode, either baseline or progressive.
fn encode(
    mode: Mode,
    src: &[u8],
    w: u32,
    h: u32,
    q: u8,
    restart: u16,
    progressive: bool,
) -> Vec<u8> {
    let script = ProgressiveScript::default();
    encode_with_script(mode, src, w, h, q, restart, progressive, script)
}

/// [`encode`] with an explicit progressive scan script.
#[allow(clippy::too_many_arguments)]
fn encode_with_script(
    mode: Mode,
    src: &[u8],
    w: u32,
    h: u32,
    q: u8,
    restart: u16,
    progressive: bool,
    script: ProgressiveScript,
) -> Vec<u8> {
    let dims = Dimensions::new(w, h).unwrap();
    let enc = JpegEncoder::new()
        .with_quality(q)
        .with_restart_interval(restart)
        .with_progressive(progressive)
        .with_progressive_script(script);
    match mode {
        Mode::Gray => enc
            .encode_to_vec(ImageRef::<Gray8>::new(src, dims).unwrap())
            .unwrap(),
        _ => enc
            .with_subsampling(mode.subsampling())
            .encode_to_vec(ImageRef::<Rgb8>::new(src, dims).unwrap())
            .unwrap(),
    }
}

/// Decodes `jpeg` with gamut into interleaved samples (1 channel gray, 3 colour).
fn decode(mode: Mode, jpeg: &[u8]) -> Vec<u8> {
    let dec = JpegDecoder::new();
    match mode {
        Mode::Gray => {
            let img: ImageBuf<Gray8> = dec.decode_image(jpeg).unwrap();
            img.as_samples().to_vec()
        }
        _ => {
            let img: ImageBuf<Rgb8> = dec.decode_image(jpeg).unwrap();
            img.as_samples().to_vec()
        }
    }
}

// ================================================================================================
// Coefficient-exactness: decode(progressive) == decode(baseline), byte for byte.
// ================================================================================================

#[test]
fn progressive_decodes_identically_to_baseline() {
    // The exactness gate over a battery: dims (incl. non-MCU-aligned) × mode × quality × restart.
    // Progressive and baseline encode the same quantized coefficients, so gamut's decoder must
    // reconstruct byte-identical images from each. Distinct per-pixel content makes any scan-geometry
    // or successive-approximation bug diverge.
    let dims = [
        (1u32, 1u32),
        (8, 8),
        (16, 16),
        (17, 9),
        (23, 19),
        (33, 31),
        (48, 40),
    ];
    for &(w, h) in &dims {
        for mode in Mode::ALL {
            for &q in &[50u8, 90] {
                for &restart in &[0u16, 2] {
                    let src: Vec<u8> = (0..(w * h * mode.channels()) as usize)
                        .map(pattern)
                        .collect();
                    let base = encode(mode, &src, w, h, q, restart, false);
                    let prog = encode(mode, &src, w, h, q, restart, true);
                    // The progressive stream really is SOF2.
                    assert_eq!(
                        gamut_jpeg::info(&prog).unwrap().process,
                        JpegProcess::Progressive,
                        "{mode:?} {w}x{h} q{q} r{restart} not progressive"
                    );
                    let a = decode(mode, &base);
                    let b = decode(mode, &prog);
                    assert_eq!(
                        a, b,
                        "{mode:?} {w}x{h} q{q} r{restart}: prog != baseline decode"
                    );
                }
            }
        }
    }
}

#[test]
fn mozjpeg_script_decodes_identically_to_baseline() {
    // The same exactness gate for the opt-in mozjpeg script: it codes the baseline coefficients in a
    // different scan split (DC whole, AC split at 8/9, chroma AC whole), so its stream must still
    // decode byte-identically to the baseline stream of the same input.
    for &(w, h) in &[(1u32, 1u32), (17, 9), (33, 31)] {
        for mode in Mode::ALL {
            for &q in &[50u8, 90] {
                for &restart in &[0u16, 2] {
                    let src: Vec<u8> = (0..(w * h * mode.channels()) as usize)
                        .map(pattern)
                        .collect();
                    let base = encode(mode, &src, w, h, q, restart, false);
                    let script = ProgressiveScript::Mozjpeg;
                    let prog = encode_with_script(mode, &src, w, h, q, restart, true, script);
                    assert_eq!(
                        decode(mode, &base),
                        decode(mode, &prog),
                        "{mode:?} {w}x{h} q{q} r{restart}: mozjpeg prog != baseline decode"
                    );
                }
            }
        }
    }
}

#[test]
fn progressive_roundtrips_flat_and_gradient_content() {
    // Flat blocks (long zero AC bands → EOB runs) and a smooth gradient (few coefficients) are the
    // content extremes for the EOB-run and refinement paths; both must decode identically to baseline.
    for &(w, h) in &[(40u32, 40u32), (24, 24)] {
        for mode in Mode::ALL {
            let ch = mode.channels() as usize;
            let flat = vec![137u8; (w * h) as usize * ch];
            let gradient: Vec<u8> = (0..(w * h) as usize * ch)
                .map(|i| ((i / ch) as u32 * 255 / (w * h)) as u8)
                .collect();
            for src in [flat, gradient] {
                let base = encode(mode, &src, w, h, 75, 0, false);
                let prog = encode(mode, &src, w, h, 75, 0, true);
                assert_eq!(decode(mode, &base), decode(mode, &prog), "{mode:?} {w}x{h}");
            }
        }
    }
}

// ================================================================================================
// Stream walker: validate the emitted progressive structure.
// ================================================================================================

/// One Huffman table parsed from a DHT segment.
struct Dht {
    class: u8,
    dest: u8,
    bits: [u8; 16],
    values: Vec<u8>,
}

/// One scan parsed from the stream: its SOS fields, the DHTs that immediately preceded it, its raw
/// entropy bytes, and how many restart markers punctuate them.
struct WalkedScan {
    comp_ids: Vec<u8>,
    td_ta: Vec<(u8, u8)>,
    ss: u8,
    se: u8,
    ah: u8,
    al: u8,
    dht_before: Vec<Dht>,
    entropy: Vec<u8>,
    rst_count: usize,
}

/// The parsed shape of a whole progressive stream.
struct Walked {
    process: u8,
    scans: Vec<WalkedScan>,
}

fn be16(b: &[u8], i: usize) -> usize {
    usize::from(u16::from_be_bytes([b[i], b[i + 1]]))
}

/// Walks a JPEG stream into its frame process and per-scan structure (SOS fields, preceding DHTs,
/// entropy bytes, restart count). Panics on anything the gamut encoder would never emit — this is a
/// test oracle, not a hardened parser.
fn walk(jpeg: &[u8]) -> Walked {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "SOI");
    let mut pos = 2;
    let mut process = 0u8;
    let mut pending: Vec<Dht> = Vec::new();
    let mut scans = Vec::new();
    loop {
        assert_eq!(jpeg[pos], 0xFF, "marker prefix at {pos}");
        let code = jpeg[pos + 1];
        pos += 2;
        match code {
            0xD9 => break, // EOI
            0xC0..=0xC2 => {
                process = code;
                pos += be16(jpeg, pos);
            }
            0xC4 => {
                // DHT: one or more (Tc/Th, BITS[16], HUFFVAL[sum]) tables.
                let len = be16(jpeg, pos);
                let end = pos + len;
                let mut p = pos + 2;
                while p < end {
                    let class = jpeg[p] >> 4;
                    let dest = jpeg[p] & 0x0F;
                    let mut bits = [0u8; 16];
                    bits.copy_from_slice(&jpeg[p + 1..p + 17]);
                    let total: usize = bits.iter().map(|&b| usize::from(b)).sum();
                    let values = jpeg[p + 17..p + 17 + total].to_vec();
                    pending.push(Dht {
                        class,
                        dest,
                        bits,
                        values,
                    });
                    p += 17 + total;
                }
                pos = end;
            }
            0xDA => {
                // SOS.
                let len = be16(jpeg, pos);
                let payload = &jpeg[pos + 2..pos + len];
                let ns = usize::from(payload[0]);
                let mut comp_ids = Vec::new();
                let mut td_ta = Vec::new();
                for j in 0..ns {
                    comp_ids.push(payload[1 + 2 * j]);
                    let x = payload[2 + 2 * j];
                    td_ta.push((x >> 4, x & 0x0F));
                }
                let ss = payload[1 + 2 * ns];
                let se = payload[2 + 2 * ns];
                let ah = payload[3 + 2 * ns] >> 4;
                let al = payload[3 + 2 * ns] & 0x0F;
                // Entropy runs until the next real (non-RST, non-stuffed) marker.
                let start = pos + len;
                let mut e = start;
                let mut rst_count = 0;
                loop {
                    if jpeg[e] == 0xFF {
                        match jpeg[e + 1] {
                            0x00 => e += 2, // stuffed literal 0xFF
                            0xFF => e += 1, // fill byte
                            m if (0xD0..=0xD7).contains(&m) => {
                                rst_count += 1;
                                e += 2;
                            }
                            _ => break, // a real marker ends the scan
                        }
                    } else {
                        e += 1;
                    }
                }
                scans.push(WalkedScan {
                    comp_ids,
                    td_ta,
                    ss,
                    se,
                    ah,
                    al,
                    dht_before: std::mem::take(&mut pending),
                    entropy: jpeg[start..e].to_vec(),
                    rst_count,
                });
                pos = e;
            }
            _ => pos += be16(jpeg, pos), // DQT / DRI / APPn / COM …
        }
    }
    Walked { process, scans }
}

/// The expected `(comp_ids, ss, se, ah, al)` for each scan of the frozen script.
fn expected_script(color: bool) -> Vec<(Vec<u8>, u8, u8, u8, u8)> {
    if color {
        vec![
            (vec![1, 2, 3], 0, 0, 0, 1),
            (vec![1], 1, 5, 0, 2),
            (vec![3], 1, 63, 0, 1),
            (vec![2], 1, 63, 0, 1),
            (vec![1], 6, 63, 0, 2),
            (vec![1], 1, 63, 2, 1),
            (vec![1, 2, 3], 0, 0, 1, 0),
            (vec![3], 1, 63, 1, 0),
            (vec![2], 1, 63, 1, 0),
            (vec![1], 1, 63, 1, 0),
        ]
    } else {
        vec![
            (vec![1], 0, 0, 0, 1),
            (vec![1], 1, 5, 0, 2),
            (vec![1], 6, 63, 0, 2),
            (vec![1], 1, 63, 2, 1),
            (vec![1], 0, 0, 1, 0),
            (vec![1], 1, 63, 1, 0),
        ]
    }
}

#[test]
fn stream_matches_the_frozen_scan_script() {
    // The emitted scan count, order, and per-scan spectral/approximation fields must be exactly
    // libjpeg's jpeg_simple_progression script, for both grayscale (6 scans) and YCbCr (10 scans).
    for &(color, w, h) in &[(false, 24u32, 24u32), (true, 24, 24)] {
        let ch = if color { 3 } else { 1 };
        let src: Vec<u8> = (0..(w * h * ch) as usize).map(pattern).collect();
        let jpeg = if color {
            let img = ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap();
            JpegEncoder::new()
                .with_progressive(true)
                .with_subsampling(ChromaSubsampling::Ycbcr420)
                .encode_to_vec(img)
                .unwrap()
        } else {
            let img = ImageRef::<Gray8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap();
            JpegEncoder::new()
                .with_progressive(true)
                .encode_to_vec(img)
                .unwrap()
        };
        let walked = walk(&jpeg);
        assert_eq!(walked.process, 0xC2, "SOF2 marker");
        let expected = expected_script(color);
        assert_eq!(walked.scans.len(), expected.len(), "scan count");
        for (scan, (ids, ss, se, ah, al)) in walked.scans.iter().zip(expected) {
            assert_eq!(scan.comp_ids, ids, "scan components");
            assert_eq!(
                (scan.ss, scan.se, scan.ah, scan.al),
                (ss, se, ah, al),
                "spectral fields"
            );
        }
    }
}

/// The expected `(comp_ids, ss, se, ah, al)` for each scan of mozjpeg's `JCP_MAX_COMPRESSION`
/// script (`dc_scan_opt_mode = 0`), written out from mozjpeg 4.1.5 `jcparam.c` as cited in
/// `references/jpeg/README.md`: the YCbCr `jpeg_scan_rgb.txt` script, or the all-purpose
/// `jpeg_scan_bw.txt` one over the frame's component `ids`.
fn expected_mozjpeg_script(ycbcr: bool, ids: &[u8]) -> Vec<(Vec<u8>, u8, u8, u8, u8)> {
    if ycbcr {
        vec![
            (vec![1, 2, 3], 0, 0, 0, 0),
            (vec![1], 1, 8, 0, 2),
            (vec![2], 1, 8, 0, 0),
            (vec![3], 1, 8, 0, 0),
            (vec![1], 9, 63, 0, 2),
            (vec![1], 1, 63, 2, 1),
            (vec![1], 1, 63, 1, 0),
            (vec![2], 9, 63, 0, 0),
            (vec![3], 9, 63, 0, 0),
        ]
    } else {
        let mut scans = vec![(ids.to_vec(), 0, 0, 0, 0)];
        for (ss, se, ah, al) in [(1, 8, 0, 2), (9, 63, 0, 2), (1, 63, 2, 1), (1, 63, 1, 0)] {
            scans.extend(ids.iter().map(|&id| (vec![id], ss, se, ah, al)));
        }
        scans
    }
}

/// Asserts that `jpeg` is SOF2 and carries exactly the `expected` scan list.
fn assert_scan_list(jpeg: &[u8], expected: &[(Vec<u8>, u8, u8, u8, u8)], what: &str) {
    let walked = walk(jpeg);
    assert_eq!(walked.process, 0xC2, "{what}: SOF2 marker");
    let got: Vec<(Vec<u8>, u8, u8, u8, u8)> = walked
        .scans
        .iter()
        .map(|s| (s.comp_ids.clone(), s.ss, s.se, s.ah, s.al))
        .collect();
    assert_eq!(got, expected, "{what}: scan list");
}

#[test]
fn stream_matches_the_mozjpeg_scan_script() {
    // The opt-in mozjpeg script, in each of its three shapes: 5 scans for grayscale, the 9-scan
    // custom script for YCbCr, and the 13-scan all-purpose script for the non-YCbCr XYB frame.
    let (w, h) = (24u32, 24u32);
    let dims = Dimensions::new(w, h).unwrap();
    let enc = JpegEncoder::new()
        .with_progressive(true)
        .with_progressive_script(ProgressiveScript::Mozjpeg);

    let gray: Vec<u8> = (0..(w * h) as usize).map(pattern).collect();
    let jpeg = enc
        .clone()
        .encode_to_vec(ImageRef::<Gray8>::new(&gray, dims).unwrap())
        .unwrap();
    assert_scan_list(&jpeg, &expected_mozjpeg_script(false, &[1]), "gray");

    let rgb: Vec<u8> = (0..(w * h * 3) as usize).map(pattern).collect();
    let jpeg = enc
        .clone()
        .with_subsampling(ChromaSubsampling::Ycbcr420)
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, dims).unwrap())
        .unwrap();
    assert_scan_list(&jpeg, &expected_mozjpeg_script(true, &[1, 2, 3]), "ycbcr");

    let jpeg = enc
        .with_color_mode(JpegColorMode::Xyb)
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, dims).unwrap())
        .unwrap();
    assert_scan_list(&jpeg, &expected_mozjpeg_script(false, b"RGB"), "xyb");
}

#[test]
fn mozjpeg_xyb_script_decodes_identically_to_baseline() {
    // The all-purpose branch is reached only by the XYB frame, so its exactness is asserted there:
    // four non-interleaved AC scans per component must still deliver every baseline coefficient.
    for &(w, h) in &[(17u32, 9u32), (33, 31)] {
        let dims = Dimensions::new(w, h).unwrap();
        let src: Vec<u8> = (0..(w * h * 3) as usize).map(pattern).collect();
        let enc = JpegEncoder::new()
            .with_color_mode(JpegColorMode::Xyb)
            .with_progressive_script(ProgressiveScript::Mozjpeg);
        let base = enc
            .clone()
            .encode_to_vec(ImageRef::<Rgb8>::new(&src, dims).unwrap())
            .unwrap();
        let prog = enc
            .with_progressive(true)
            .encode_to_vec(ImageRef::<Rgb8>::new(&src, dims).unwrap())
            .unwrap();
        assert_eq!(
            decode(Mode::C444, &base),
            decode(Mode::C444, &prog),
            "{w}x{h}"
        );
    }
}

#[test]
fn each_scan_defines_exactly_the_table_it_references() {
    // A table-bearing scan (all but the DC-refinement scans) must be immediately preceded by a single
    // DHT defining destination 0 of the class it uses (0 = DC for Ss=0, 1 = AC otherwise); a
    // DC-refinement scan (Ss=0, Ah≠0) carries no DHT and reuses the DC table an earlier scan defined.
    let (w, h) = (24u32, 24u32);
    let src: Vec<u8> = (0..(w * h * 3) as usize).map(pattern).collect();
    let img = ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_progressive(true)
        .with_subsampling(ChromaSubsampling::Ycbcr420)
        .encode_to_vec(img)
        .unwrap();
    let walked = walk(&jpeg);
    for scan in &walked.scans {
        let is_dc = scan.ss == 0;
        let is_dc_refine = is_dc && scan.ah != 0;
        if is_dc_refine {
            assert!(
                scan.dht_before.is_empty(),
                "DC-refine scan must carry no DHT"
            );
        } else {
            assert_eq!(scan.dht_before.len(), 1, "one table per scan");
            let dht = &scan.dht_before[0];
            assert_eq!(dht.class, u8::from(!is_dc), "table class matches band");
            assert_eq!(dht.dest, 0, "table destination 0");
            // Every scan component references that destination for its class.
            for &(td, ta) in &scan.td_ta {
                let used = if is_dc { td } else { ta };
                assert_eq!(used, 0, "component references destination 0");
            }
            // The table is well-formed: sum(BITS) == len(HUFFVAL), no length > 16.
            let total: usize = dht.bits.iter().map(|&b| usize::from(b)).sum();
            assert_eq!(total, dht.values.len(), "sum(BITS) == len(HUFFVAL)");
        }
    }
}

#[test]
fn restart_markers_punctuate_each_scan_at_the_declared_cadence() {
    // With a restart interval R, every scan emits a RSTn every R of its own MCUs. The MCU count
    // differs per scan (an interleaved DC scan walks the padded whole-image MCU grid; a
    // single-component AC scan walks that component's block grid), so the expected restart count is
    // derived per scan from its geometry.
    let (w, h) = (40u32, 40u32);
    let restart = 3u16;
    let src: Vec<u8> = (0..(w * h * 3) as usize).map(pattern).collect();
    let img = ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_progressive(true)
        .with_subsampling(ChromaSubsampling::Ycbcr420) // hmax=vmax=2
        .with_restart_interval(restart)
        .encode_to_vec(img)
        .unwrap();
    let walked = walk(&jpeg);
    // Geometry for 40×40 4:2:0: interleaved MCUs = ceil(40/16)² = 3×3 = 9; luma block grid = 5×5 = 25;
    // chroma block grid = ceil(20/8)² = 3×3 = 9.
    let interleaved_mcus = 9usize;
    let luma_mcus = 25usize;
    let chroma_mcus = 9usize;
    let expect_rst = |mcus: usize| (mcus - 1) / usize::from(restart);
    for scan in &walked.scans {
        let mcus = if scan.comp_ids.len() > 1 {
            interleaved_mcus // interleaved DC scan
        } else if scan.comp_ids[0] == 1 {
            luma_mcus
        } else {
            chroma_mcus
        };
        assert_eq!(
            scan.rst_count,
            expect_rst(mcus),
            "restart cadence for scan {:?}",
            scan.comp_ids
        );
    }
}

// ================================================================================================
// EOB-run paths: an EOBn symbol with run ≥ 1 is actually emitted on flat content.
// ================================================================================================

/// Decodes the first Huffman symbol of `entropy` against a canonical table built from `(bits,
/// values)`. Used to confirm the EOB-run machinery emits a real `EOBn` (run ≥ 1) symbol.
fn first_symbol(bits: &[u8; 16], values: &[u8], entropy: &[u8]) -> u8 {
    // Canonical (code, length, symbol) assignment (§C.2).
    let mut table: Vec<(u16, u8, u8)> = Vec::new();
    let mut code = 0u16;
    let mut k = 0usize;
    for (li, &count) in bits.iter().enumerate() {
        for _ in 0..count {
            table.push((code, (li + 1) as u8, values[k]));
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    // De-stuff the entropy bytes (`0xFF 0x00` → literal `0xFF`) so the bits can be read directly.
    let mut destuffed = Vec::new();
    let mut i = 0;
    while i < entropy.len() {
        destuffed.push(entropy[i]);
        if entropy[i] == 0xFF && entropy.get(i + 1) == Some(&0x00) {
            i += 2;
        } else {
            i += 1;
        }
    }
    // MSB-first bit read, matching the smallest possible canonical code.
    let mut acc = 0u16;
    for len in 1..=16u8 {
        let bitpos = usize::from(len - 1);
        let bit = (destuffed[bitpos / 8] >> (7 - bitpos % 8)) & 1;
        acc = (acc << 1) | u16::from(bit);
        if let Some(&(_, _, sym)) = table.iter().find(|&&(c, l, _)| l == len && c == acc) {
            return sym;
        }
    }
    panic!("no symbol decoded from entropy");
}

#[test]
fn flat_content_emits_a_nonzero_eob_run() {
    // A perfectly flat image has an all-zero AC band in every block, so the low-luma AC first-pass
    // scan (Ss=1, Se=5, Ah=0) is one long EOB run: its first entropy symbol must be an EOBn with a
    // run nibble ≥ 1 (not EOB0, not ZRL) — proof the EOBn accumulation path is exercised.
    let (w, h) = (40u32, 40u32); // 5×5 = 25 luma blocks, all empty-band
    let src = vec![90u8; (w * h) as usize];
    let img = ImageRef::<Gray8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_progressive(true)
        .encode_to_vec(img)
        .unwrap();
    let walked = walk(&jpeg);
    // The low-luma AC first-pass scan: Ss=1, Se=5, Ah=0.
    let scan = walked
        .scans
        .iter()
        .find(|s| (s.ss, s.se, s.ah) == (1, 5, 0))
        .expect("low AC scan");
    let dht = &scan.dht_before[0];
    let sym = first_symbol(&dht.bits, &dht.values, &scan.entropy);
    let run = sym >> 4;
    let size = sym & 0x0F;
    assert!(
        sym != 0xF0 && size == 0 && run >= 1,
        "expected EOBn run≥1, got {sym:#04x}"
    );
    // The run must be exactly the 25 empty blocks: EOBn nbits = floor(log2(25)) = 4 → symbol 0x40.
    assert_eq!(sym, 0x40, "EOB run of 25 blocks encodes as nbits=4");

    // The EOB-run accumulation must also fire in a refinement scan: on flat content no coefficient
    // ever becomes nonzero, so an AC refinement scan (Ah ≠ 0) is likewise one long EOB run whose
    // first symbol is an EOBn with run ≥ 1 — pinning the refinement path's EOBRUN accumulation.
    let refine = walked
        .scans
        .iter()
        .find(|s| s.ah != 0 && s.se == 63)
        .expect("an AC refinement scan");
    let rdht = &refine.dht_before[0];
    let rsym = first_symbol(&rdht.bits, &rdht.values, &refine.entropy);
    assert!(
        rsym != 0xF0 && (rsym & 0x0F) == 0 && (rsym >> 4) >= 1,
        "refinement scan expected EOBn run≥1, got {rsym:#04x}"
    );
}

#[test]
fn eob_run_caps_at_its_15_bit_maximum() {
    // The EOB run is forced out at 0x7FFF (§G.1.2.2): a flat image with more than 32767 empty-band
    // blocks in a single scan drives the accumulator to its cap mid-scan, exercising the forced flush
    // in both the AC first-pass and refinement models. Without the cap the run overflows into an
    // invalid EOBn (an all-ones run nibble = ZRL), so decode parity with baseline would break — this
    // is the test that kills a disabled or mis-valued cap. 1456×1456 → 182×182 = 33124 luma blocks.
    let (w, h) = (1456u32, 1456u32);
    let src = vec![119u8; (w * h) as usize];
    let img = ImageRef::<Gray8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap();
    let base = JpegEncoder::new().encode_to_vec(img).unwrap();
    let prog = JpegEncoder::new()
        .with_progressive(true)
        .encode_to_vec(img)
        .unwrap();
    let a: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&base).unwrap();
    let b: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&prog).unwrap();
    assert_eq!(a.as_samples(), b.as_samples());
}

#[test]
fn eob_run_resets_at_restart_boundaries() {
    // With restarts, an EOB run may not cross a RSTn (§G.1.2.2). A flat image with a restart interval
    // forces the run to flush and restart at each boundary; decode parity with baseline confirms the
    // encoder and decoder agree on where the runs break.
    let (w, h) = (64u32, 8u32); // 8 luma blocks in a row
    let src = vec![200u8; (w * h) as usize];
    let img = ImageRef::<Gray8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap();
    let base = JpegEncoder::new()
        .with_restart_interval(2)
        .encode_to_vec(img)
        .unwrap();
    let prog = JpegEncoder::new()
        .with_restart_interval(2)
        .with_progressive(true)
        .encode_to_vec(img)
        .unwrap();
    let a: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&base).unwrap();
    let b: ImageBuf<Gray8> = JpegDecoder::new().decode_image(&prog).unwrap();
    assert_eq!(a.as_samples(), b.as_samples());
    // The AC scans really do carry restart markers at the R=2 cadence over 8 blocks: (8-1)/2 = 3.
    let walked = walk(&prog);
    let ac = walked
        .scans
        .iter()
        .find(|s| s.ss == 1 && s.se == 5)
        .unwrap();
    assert_eq!(ac.rst_count, 3);
}

#[test]
fn custom_quant_tables_feed_baseline_and_progressive_identically() {
    // `with_quant_tables` swaps the tables at the shared `luma_quant`/`chroma_quant` chokepoints,
    // so the progressive path must pick up exactly the tables the baseline path quantizes with —
    // decode(progressive) == decode(baseline) byte-for-byte under a custom pair distinct from any
    // quality-scaled Annex K output.
    use gamut_jpeg::QuantTables;
    let tables = QuantTables::new([6u8; 64], [14u8; 64]).unwrap();
    let (w, h) = (23u32, 19u32);
    for mode in [Mode::Gray, Mode::C420] {
        let src: Vec<u8> = (0..(w * h * mode.channels()) as usize)
            .map(pattern)
            .collect();
        let dims = Dimensions::new(w, h).unwrap();
        let enc = |progressive: bool| {
            let e = JpegEncoder::new()
                .with_quant_tables(tables)
                .with_progressive(progressive);
            match mode {
                Mode::Gray => e
                    .encode_to_vec(ImageRef::<Gray8>::new(&src, dims).unwrap())
                    .unwrap(),
                _ => e
                    .with_subsampling(mode.subsampling())
                    .encode_to_vec(ImageRef::<Rgb8>::new(&src, dims).unwrap())
                    .unwrap(),
            }
        };
        let base = enc(false);
        let prog = enc(true);
        assert_eq!(
            gamut_jpeg::info(&prog).unwrap().process,
            JpegProcess::Progressive
        );
        assert_eq!(
            decode(mode, &base),
            decode(mode, &prog),
            "{mode:?}: custom-table prog != baseline decode"
        );
    }
}
