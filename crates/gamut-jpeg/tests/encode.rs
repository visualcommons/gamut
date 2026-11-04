//! Integration tests for the baseline JPEG encoder: a structural stream walker run across a
//! dimension/subsampling/restart battery, plus byte-exact micro-goldens hand-derived from T.81
//! Annex F/K. These exercise only the public API and validate the raw bytes, so they pin the
//! marker serialization and entropy framing against mutation.

use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef, Rgb8};
use gamut_jpeg::{ChromaSubsampling, JpegEncoder};

/// A parsed baseline JPEG: the header marker segments (in order), the SOF0/SOS fields, and the
/// entropy region (raw bytes between the SOS header and EOI, plus the restart-marker sequence).
struct Parsed {
    /// `(marker_code, payload)` for each header marker segment up to and including SOS.
    segments: Vec<(u8, Vec<u8>)>,
    /// Entropy-coded bytes between the SOS header and EOI, restart markers removed but stuffing kept.
    entropy: Vec<u8>,
    /// The `m` value of each RSTn marker encountered, in order.
    rst_sequence: Vec<u8>,
}

impl Parsed {
    fn segment(&self, code: u8) -> Option<&[u8]> {
        self.segments
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, p)| p.as_slice())
    }
}

/// Marker codes referenced by the walker.
mod m {
    pub const SOI: u8 = 0xD8;
    pub const EOI: u8 = 0xD9;
    pub const SOF0: u8 = 0xC0;
    pub const DHT: u8 = 0xC4;
    pub const DQT: u8 = 0xDB;
    pub const DRI: u8 = 0xDD;
    pub const SOS: u8 = 0xDA;
    pub const APP0: u8 = 0xE0;
}

/// Parses and structurally validates a baseline JPEG, panicking with a message on any inconsistency.
/// Checks: SOI first / EOI last, every segment length self-consistent and in-bounds, and every
/// `0xFF` inside the entropy data followed by `0x00` (stuffing) or an RSTn marker.
fn parse(data: &[u8]) -> Parsed {
    assert!(data.len() >= 4, "stream too short");
    assert_eq!(&data[..2], &[0xFF, m::SOI], "must start with SOI");
    assert_eq!(
        &data[data.len() - 2..],
        &[0xFF, m::EOI],
        "must end with EOI"
    );

    let mut segments = Vec::new();
    let mut pos = 2;
    // Header marker segments, up to and including SOS.
    loop {
        assert_eq!(data[pos], 0xFF, "expected a marker at {pos}");
        let code = data[pos + 1];
        pos += 2;
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        assert!(len >= 2, "segment length must include its own 2 bytes");
        assert!(pos + len <= data.len(), "segment {code:#x} runs past end");
        let payload = data[pos + 2..pos + len].to_vec();
        segments.push((code, payload));
        pos += len;
        if code == m::SOS {
            break;
        }
    }

    // Entropy-coded data until EOI.
    let mut entropy = Vec::new();
    let mut rst_sequence = Vec::new();
    loop {
        let byte = data[pos];
        if byte == 0xFF {
            let next = data[pos + 1];
            match next {
                0x00 => {
                    entropy.push(0xFF); // a stuffed literal 0xFF
                    pos += 2;
                }
                0xD0..=0xD7 => {
                    rst_sequence.push(next - 0xD0);
                    pos += 2;
                }
                m::EOI => {
                    pos += 2;
                    break;
                }
                other => panic!("unexpected marker {other:#x} inside entropy data"),
            }
        } else {
            entropy.push(byte);
            pos += 1;
        }
    }
    assert_eq!(pos, data.len(), "trailing bytes after EOI");

    Parsed {
        segments,
        entropy,
        rst_sequence,
    }
}

/// The ordered marker codes of the header segments.
fn marker_order(p: &Parsed) -> Vec<u8> {
    p.segments.iter().map(|(c, _)| *c).collect()
}

/// Splits a DHT payload (§B.2.4.2) into its `(Tc|Th, BITS, HUFFVAL)` tables.
fn dht_tables(payload: &[u8]) -> Vec<(u8, [u8; 16], Vec<u8>)> {
    let mut tables = Vec::new();
    let mut i = 0;
    while i < payload.len() {
        let tc_th = payload[i];
        let mut bits = [0u8; 16];
        bits.copy_from_slice(&payload[i + 1..i + 17]);
        let count: usize = bits.iter().map(|&b| usize::from(b)).sum();
        assert!(i + 17 + count <= payload.len(), "HUFFVAL runs past the DHT");
        tables.push((tc_th, bits, payload[i + 17..i + 17 + count].to_vec()));
        i += 17 + count;
    }
    tables
}

#[test]
fn header_order_and_fields_grayscale() {
    let img = ImageRef::<Gray8>::new(&[128u8; 64], Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new().encode_to_vec(img).unwrap();
    let p = parse(&jpeg);

    // Emit order: APP0, DQT, SOF0, DHT, SOS (no DRI — default has no restart).
    assert_eq!(
        marker_order(&p),
        vec![m::APP0, m::DQT, m::SOF0, m::DHT, m::SOS]
    );

    // APP0 begins with the JFIF identifier.
    assert_eq!(&p.segment(m::APP0).unwrap()[..5], b"JFIF\0");

    // SOF0: P=8, Y=8, X=8, Nf=1, and the single component is Y (id 1) at 1×1 sampling, quant 0.
    let sof = p.segment(m::SOF0).unwrap();
    assert_eq!(sof[0], 8, "precision");
    assert_eq!(&sof[1..3], &8u16.to_be_bytes(), "Y=height");
    assert_eq!(&sof[3..5], &8u16.to_be_bytes(), "X=width");
    assert_eq!(sof[5], 1, "Nf");
    assert_eq!(&sof[6..9], &[1, 0x11, 0]);

    // SOS: Ns=1, then the baseline spectral fields Ss=0, Se=63, Ah=Al=0.
    let sos = p.segment(m::SOS).unwrap();
    assert_eq!(sos[0], 1, "Ns");
    assert_eq!(&sos[sos.len() - 3..], &[0, 63, 0], "Ss, Se, Ah|Al");
}

#[test]
fn header_order_color_with_restart_has_dri() {
    let rgb = vec![10u8; 16 * 16 * 3];
    let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(16, 16).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_restart_interval(2)
        .encode_to_vec(img)
        .unwrap();
    let p = parse(&jpeg);
    // DRI appears (before SOS) exactly when a restart interval is set.
    assert_eq!(
        marker_order(&p),
        vec![m::APP0, m::DQT, m::SOF0, m::DHT, m::DRI, m::SOS]
    );
    // DRI payload is the 2-byte interval.
    assert_eq!(p.segment(m::DRI).unwrap(), &2u16.to_be_bytes());
    // SOF0 declares 3 components; Y at 2×2 (default 4:2:0), chroma at 1×1.
    let sof = p.segment(m::SOF0).unwrap();
    assert_eq!(sof[5], 3, "Nf");
    assert_eq!(&sof[6..9], &[1, 0x22, 0]); // Y
    assert_eq!(&sof[9..12], &[2, 0x11, 1]); // Cb
    assert_eq!(&sof[12..15], &[3, 0x11, 1]); // Cr
}

#[test]
fn golden_constant_gray_8x8_entropy_is_hand_derived() {
    // A single 8×8 block. At quality 50 the luma DC quant step Q00 = 16 (Annex K.1 verbatim).
    //
    //   value 128 → level-shifted 0 → S00 = 0 → quantized DC = 0 → DC diff 0 (category 0).
    //     DC code = luma-DC symbol 0 = "00" (2 bits); AC all zero → EOB "1010" (4 bits).
    //     bits = 00 1010, padded with two 1s → 0b0010_1011 = 0x2B.
    let img = ImageRef::<Gray8>::new(&[128u8; 64], Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_quality(50)
        .encode_to_vec(img)
        .unwrap();
    assert_eq!(parse(&jpeg).entropy, vec![0x2B]);

    //   value 255 → level-shifted 127 → S00 = 1016 → quantized DC = round(1016/16) = 64.
    //     DC diff 64 → category 7; luma-DC symbol 7 = "11110" (5 bits); magnitude = 64 in 7 bits
    //     = "1000000". AC EOB "1010" (4 bits). bits = 11110 1000000 1010 = 16 bits exactly →
    //     0b1111_0100 0b0000_1010 = 0xF4, 0x0A (no padding, no 0xFF so no stuffing).
    let img = ImageRef::<Gray8>::new(&[255u8; 64], Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_quality(50)
        .encode_to_vec(img)
        .unwrap();
    assert_eq!(parse(&jpeg).entropy, vec![0xF4, 0x0A]);
}

#[test]
fn optimized_tables_replace_the_standard_dht_without_touching_the_layout() {
    // Optimized tables are a pure entropy-coding change: same markers, same one DHT segment in the
    // same place, same DQT/SOF0/SOS bytes — only the table contents and the entropy data differ.
    let rgb: Vec<u8> = (0..48 * 32 * 3)
        .map(|i| ((i * 71 + 13) % 256) as u8)
        .collect();
    let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(48, 32).unwrap()).unwrap();

    let fixed = JpegEncoder::new()
        .with_quality(80)
        .encode_to_vec(img)
        .unwrap();
    let optimized = JpegEncoder::new()
        .with_quality(80)
        .with_optimized_tables(true)
        .encode_to_vec(img)
        .unwrap();
    let (pf, po) = (parse(&fixed), parse(&optimized));

    assert_eq!(marker_order(&po), marker_order(&pf), "marker layout");
    assert_eq!(
        marker_order(&po).iter().filter(|&&c| c == m::DHT).count(),
        1,
        "still a single DHT segment"
    );
    assert_eq!(po.segment(m::DQT), pf.segment(m::DQT));
    assert_eq!(po.segment(m::SOF0), pf.segment(m::SOF0));
    assert_eq!(po.segment(m::SOS), pf.segment(m::SOS));

    let tf = dht_tables(pf.segment(m::DHT).unwrap());
    let to = dht_tables(po.segment(m::DHT).unwrap());
    assert_eq!(
        to.iter().map(|t| t.0).collect::<Vec<_>>(),
        vec![0x00, 0x10, 0x01, 0x11],
        "luma DC/AC then chroma DC/AC, the fixed-table order"
    );
    assert_eq!(
        to.iter().map(|t| t.0).collect::<Vec<_>>(),
        tf.iter().map(|t| t.0).collect::<Vec<_>>()
    );
    for (o, f) in to.iter().zip(&tf) {
        assert!(
            (o.1, &o.2) != (f.1, &f.2),
            "destination {:#04x} still carries the Annex K table",
            o.0
        );
    }

    assert!(
        optimized.len() < fixed.len(),
        "optimized {} bytes vs fixed {} bytes",
        optimized.len(),
        fixed.len()
    );
}

#[test]
fn optimized_grayscale_emits_only_the_two_luma_tables() {
    // A grayscale scan never references the chroma destinations, so the optimized DHT must carry
    // exactly the luma DC and AC tables — matching what the fixed-table path writes for gray.
    let gray: Vec<u8> = (0..32 * 32).map(|i| ((i * 37 + 5) % 256) as u8).collect();
    let img = ImageRef::<Gray8>::new(&gray, Dimensions::new(32, 32).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_optimized_tables(true)
        .encode_to_vec(img)
        .unwrap();
    let tables = dht_tables(parse(&jpeg).segment(m::DHT).unwrap());
    assert_eq!(
        tables.iter().map(|t| t.0).collect::<Vec<_>>(),
        vec![0x00, 0x10]
    );
}

#[test]
fn optimized_tables_survive_restart_intervals_and_flat_content() {
    // Restart markers reset the DC predictors mid-scan, so the gather pass must see exactly the
    // symbols the emit pass writes. Flat content is the degenerate end: almost every block codes a
    // zero DC difference and an immediate EOB, leaving very sparse histograms.
    for &restart in &[0u16, 1, 3] {
        let rgb = vec![97u8; 40 * 24 * 3];
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(40, 24).unwrap()).unwrap();
        let jpeg = JpegEncoder::new()
            .with_optimized_tables(true)
            .with_restart_interval(restart)
            .encode_to_vec(img)
            .unwrap();
        // `parse` structurally validates stuffing, segment lengths and the restart-marker framing.
        let p = parse(&jpeg);
        assert!(
            !dht_tables(p.segment(m::DHT).unwrap()).is_empty(),
            "restart {restart}: a table is always emitted"
        );
    }
}

/// The luma `(Hmax, Vmax)` sampling of a subsampling mode — the MCU is `8·Hmax × 8·Vmax` pixels.
fn luma_max(s: ChromaSubsampling) -> (u32, u32) {
    match s {
        ChromaSubsampling::Ycbcr444 => (1, 1),
        ChromaSubsampling::Ycbcr422 => (2, 1),
        ChromaSubsampling::Ycbcr420 => (2, 2),
        ChromaSubsampling::Ycbcr440 => (1, 2),
        _ => unreachable!(),
    }
}

#[test]
fn dimension_and_subsampling_battery() {
    let dims = [(1, 1), (7, 7), (8, 8), (9, 9), (16, 16), (17, 9), (64, 48)];
    let subs = [
        ChromaSubsampling::Ycbcr444,
        ChromaSubsampling::Ycbcr422,
        ChromaSubsampling::Ycbcr420,
        ChromaSubsampling::Ycbcr440,
    ];
    for &(w, h) in &dims {
        for &restart in &[0u16, 1, 3] {
            // Grayscale (subsampling irrelevant → 8×8 MCUs).
            let gray: Vec<u8> = (0..w * h).map(|i| (i * 37 % 256) as u8).collect();
            let img = ImageRef::<Gray8>::new(&gray, Dimensions::new(w, h).unwrap()).unwrap();
            let jpeg = JpegEncoder::new()
                .with_restart_interval(restart)
                .encode_to_vec(img)
                .unwrap();
            let p = parse(&jpeg);
            let mcus_x = w.div_ceil(8);
            let mcus_y = h.div_ceil(8);
            check_restarts(
                &p,
                mcus_x * mcus_y,
                restart,
                &format!("gray {w}x{h} r{restart}"),
            );

            // Colour, each subsampling.
            for &sub in &subs {
                let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 53 % 256) as u8).collect();
                let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap();
                let jpeg = JpegEncoder::new()
                    .with_subsampling(sub)
                    .with_restart_interval(restart)
                    .encode_to_vec(img)
                    .unwrap();
                let p = parse(&jpeg);
                let (hmax, vmax) = luma_max(sub);
                let mcus_x = w.div_ceil(8 * hmax);
                let mcus_y = h.div_ceil(8 * vmax);
                check_restarts(
                    &p,
                    mcus_x * mcus_y,
                    restart,
                    &format!("color {w}x{h} {sub:?} r{restart}"),
                );
            }
        }
    }
}

/// Verifies the RSTn cadence for a scan of `total_mcus` MCUs at restart interval `restart`: a
/// restart marker falls after every `restart` MCUs except the final partial interval, and the
/// markers cycle 0,1,2,…,7,0,….
fn check_restarts(p: &Parsed, total_mcus: u32, restart: u16, ctx: &str) {
    let expected_count = if restart == 0 {
        0
    } else {
        // Markers at MCU indices restart, 2·restart, … that are strictly below total_mcus.
        (total_mcus.saturating_sub(1)) / u32::from(restart)
    };
    assert_eq!(
        p.rst_sequence.len() as u32,
        expected_count,
        "{ctx}: RST count"
    );
    for (i, &m) in p.rst_sequence.iter().enumerate() {
        assert_eq!(m, (i % 8) as u8, "{ctx}: RST cycling at #{i}");
    }
}

#[test]
fn quality_is_clamped_not_rejected() {
    // Out-of-range qualities clamp to 1..=100 (matching libjpeg): both extremes still encode.
    let img = ImageRef::<Gray8>::new(&[100u8; 64], Dimensions::new(8, 8).unwrap()).unwrap();
    for q in [0u8, 1, 200, 255] {
        let jpeg = JpegEncoder::new()
            .with_quality(q)
            .encode_to_vec(img)
            .unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }
    // Quality 0 clamps to 1 (coarsest); its DQT differs from quality 100 (all-1 tables).
    let q0 = JpegEncoder::new()
        .with_quality(0)
        .encode_to_vec(img)
        .unwrap();
    let q100 = JpegEncoder::new()
        .with_quality(100)
        .encode_to_vec(img)
        .unwrap();
    assert_ne!(parse(&q0).segment(m::DQT), parse(&q100).segment(m::DQT));
}

#[test]
fn oversized_dimension_is_rejected() {
    // 65536 exceeds the 16-bit SOF0 X/Y field — on either axis (pinning that BOTH dimensions are
    // checked, in their own positions).
    let long = vec![0u8; 65536];
    let wide = ImageRef::<Gray8>::new(&long, Dimensions::new(65536, 1).unwrap()).unwrap();
    let mut out = Vec::new();
    assert!(JpegEncoder::new().encode_image(wide, &mut out).is_err());
    let tall = ImageRef::<Gray8>::new(&long, Dimensions::new(1, 65536).unwrap()).unwrap();
    let mut out = Vec::new();
    assert!(JpegEncoder::new().encode_image(tall, &mut out).is_err());
}

#[test]
fn max_dimension_65535_encodes() {
    // 65535 is the LARGEST legal SOF0 dimension (Table B.2: X in 1–65535, Y up to 65535) — it must
    // encode on either axis, pinning the strict `>` limit against an off-by-one `>=`.
    let edge = vec![128u8; 65535];
    let wide = ImageRef::<Gray8>::new(&edge, Dimensions::new(65535, 1).unwrap()).unwrap();
    let jpeg = JpegEncoder::new().encode_to_vec(wide).unwrap();
    let p = parse(&jpeg);
    assert_eq!(&p.segment(m::SOF0).unwrap()[3..5], &65535u16.to_be_bytes()); // X = width
    let tall = ImageRef::<Gray8>::new(&edge, Dimensions::new(1, 65535).unwrap()).unwrap();
    let jpeg = JpegEncoder::new().encode_to_vec(tall).unwrap();
    let p = parse(&jpeg);
    assert_eq!(&p.segment(m::SOF0).unwrap()[1..3], &65535u16.to_be_bytes()); // Y = height
}

#[test]
fn byte_count_is_relative_to_appended_output() {
    // `encode_image` appends and returns only the bytes IT wrote — encode into a Vec that already
    // holds a 5-byte prefix and check the count and the intact prefix, for both pixel impls.
    let prefix = [0xA1u8, 0xA2, 0xA3, 0xA4, 0xA5];

    let gray = ImageRef::<Gray8>::new(&[90u8; 64], Dimensions::new(8, 8).unwrap()).unwrap();
    let mut out = prefix.to_vec();
    let n = JpegEncoder::new().encode_image(gray, &mut out).unwrap();
    assert_eq!(n, out.len() - 5);
    assert_eq!(&out[..5], &prefix);
    assert_eq!(&out[5..7], &[0xFF, 0xD8]); // SOI right after the prefix

    let rgb = vec![70u8; 8 * 8 * 3];
    let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(8, 8).unwrap()).unwrap();
    let mut out = prefix.to_vec();
    let n = JpegEncoder::new().encode_image(img, &mut out).unwrap();
    assert_eq!(n, out.len() - 5);
    assert_eq!(&out[..5], &prefix);
    assert_eq!(&out[5..7], &[0xFF, 0xD8]);
}

#[test]
fn density_is_written_to_app0() {
    use gamut_jpeg::DensityUnit;
    let img = ImageRef::<Gray8>::new(&[0u8; 64], Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_density(DensityUnit::Dpi, 300, 150)
        .encode_to_vec(img)
        .unwrap();
    let parsed = parse(&jpeg);
    let app0 = parsed.segment(m::APP0).unwrap();
    // identifier(5) version(2) units(1) then Hdensity(2) Vdensity(2).
    assert_eq!(app0[7], 1, "units = dpi");
    assert_eq!(&app0[8..10], &300u16.to_be_bytes());
    assert_eq!(&app0[10..12], &150u16.to_be_bytes());
}

#[test]
fn custom_quant_tables_reach_the_dqt_verbatim_and_quality_is_inert() {
    use gamut_jpeg::QuantTables;
    // Distinct constant tables: a luma/chroma swap or an accidental quality re-scale of either
    // half would change the payload bytes. (Zig-zag re-emission of a non-constant table is pinned
    // by the quant module's own DQT test; constants make the whole 64 assertable here.)
    let tables = QuantTables::new([7u8; 64], [11u8; 64]).unwrap();
    let rgb = vec![100u8; 16 * 16 * 3];
    let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(16, 16).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_quality(30) // must be ignored: custom tables bypass the quality mapping
        .with_quant_tables(tables)
        .encode_to_vec(img)
        .unwrap();
    let p = parse(&jpeg);
    let dqt = p.segment(m::DQT).unwrap();
    let mut expected = vec![0x00u8]; // Pq=0 | Tq=0
    expected.extend_from_slice(&[7u8; 64]);
    expected.push(0x01); // Pq=0 | Tq=1
    expected.extend_from_slice(&[11u8; 64]);
    assert_eq!(dqt, expected.as_slice());

    // With custom tables set, quality must be inert for the WHOLE stream, not just the DQT.
    let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(16, 16).unwrap()).unwrap();
    let q90 = JpegEncoder::new()
        .with_quality(90)
        .with_quant_tables(tables)
        .encode_to_vec(img)
        .unwrap();
    assert_eq!(jpeg, q90);

    // Grayscale uses only the luma table: a single 65-byte DQT payload at Tq=0.
    let gray = ImageRef::<Gray8>::new(&[100u8; 64], Dimensions::new(8, 8).unwrap()).unwrap();
    let jpeg = JpegEncoder::new()
        .with_quant_tables(tables)
        .encode_to_vec(gray)
        .unwrap();
    let p = parse(&jpeg);
    let dqt = p.segment(m::DQT).unwrap();
    let mut expected = vec![0x00u8];
    expected.extend_from_slice(&[7u8; 64]);
    assert_eq!(dqt, expected.as_slice());
}

#[test]
fn annex_k_scaled_custom_tables_reproduce_the_quality_path_byte_for_byte() {
    use gamut_jpeg::QuantTables;
    // `QuantTables::annex_k().scaled(q)` is documented to be exactly the pair `with_quality(q)`
    // uses, so the two configurations must produce byte-identical streams — bridging the custom
    // path to the frozen quality contract. Gray and colour, quality on both sides of 50.
    let rgb = vec![90u8; 24 * 16 * 3];
    for &q in &[25u8, 85] {
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(24, 16).unwrap()).unwrap();
        let via_quality = JpegEncoder::new()
            .with_quality(q)
            .encode_to_vec(img)
            .unwrap();
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(24, 16).unwrap()).unwrap();
        let via_tables = JpegEncoder::new()
            .with_quant_tables(QuantTables::annex_k().scaled(q))
            .encode_to_vec(img)
            .unwrap();
        assert_eq!(via_quality, via_tables, "q{q}: custom-table path diverged");
    }
}

#[test]
fn rd_none_is_byte_identical_to_the_default() {
    use gamut_jpeg::RdOptimization;
    // `RdOptimization::None` IS the default path — not merely equivalent: byte-for-byte.
    let rgb = vec![77u8; 24 * 16 * 3];
    let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(24, 16).unwrap()).unwrap();
    let default = JpegEncoder::new().encode_to_vec(img).unwrap();
    let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(24, 16).unwrap()).unwrap();
    let explicit = JpegEncoder::new()
        .with_rd_optimization(RdOptimization::None)
        .encode_to_vec(img)
        .unwrap();
    assert_eq!(default, explicit);
}
