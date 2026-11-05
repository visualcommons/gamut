//! Differential check of the decoder's framing and header layer against **libaom**, the AV1
//! reference codec (issue #259, slice D3).
//!
//! `crates/gamut-av1/tests/recon.rs` proves the *encoder* by decoding its output with libaom and
//! dav1d. This is the mirror direction, and the one #259 exists for: libaom's reference
//! **encoder** produces conformant AV1 stills, and gamut's decoder must make sense of them.
//! `references/av1/README.md` stages `aom_oracle::encode_still_intra` for exactly this.
//!
//! It matters that the streams come from libaom rather than from gamut's own encoder: libaom
//! chooses tools gamut never emits (its own partition and transform decisions, its own tile
//! layout, `enable_intra_edge_filter`, screen-content detection), so parsing its headers exercises
//! syntax paths the encoder's round-trip cannot reach.
//!
//! The tile *body* is not decoded yet, so each case asserts that
//! [`Av1Decoder::inspect`](gamut_av1::Av1Decoder::inspect) succeeds — every byte from the OBU
//! header through the tile-size prefixes was parsed — and that the geometry, depth, chroma and
//! losslessness it reports agree with what libaom's own decoder says about the same stream.
//! When the tile body lands, these cases gain a sample comparison against
//! `aom_oracle::decode_av1`.

use gamut_av1::Av1Decoder;
use gamut_color::ChromaSubsampling;
use gamut_core::DecodeLimits;

/// The `[Y, U, V]` planes `aom_oracle::encode_still_intra` takes, at full 4:4:4 resolution.
type Planes = (Vec<u8>, Vec<u8>, Vec<u8>);

/// Builds `width × height` 4:4:4 planes from a per-pixel generator.
fn planes(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Planes {
    let n = (width * height) as usize;
    let (mut y, mut u, mut v) = (vec![0u8; n], vec![0u8; n], vec![0u8; n]);
    for row in 0..height {
        for col in 0..width {
            let i = (row * width + col) as usize;
            let [a, b, c] = f(col, row);
            y[i] = a;
            u[i] = b;
            v[i] = c;
        }
    }
    (y, u, v)
}

/// A flat mid-grey frame — the degenerate case, all-DC and heavily skipped.
fn flat(width: u32, height: u32) -> Planes {
    planes(width, height, |_, _| [128, 128, 128])
}

/// A smooth two-axis gradient, which drives directional and smooth intra modes.
fn gradient(width: u32, height: u32) -> Planes {
    planes(width, height, |x, y| {
        [(x * 3) as u8, (y * 5) as u8, (x ^ y) as u8]
    })
}

/// Deterministic pseudo-random noise, which defeats prediction and maximises coefficient coding.
fn noise(width: u32, height: u32) -> Planes {
    planes(width, height, |x, y| {
        let mut h = u64::from(x).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(y) << 32;
        h ^= h >> 29;
        h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        [(h >> 24) as u8, (h >> 32) as u8, (h >> 40) as u8]
    })
}

/// Hard-edged blocks of flat colour — the shape that triggers libaom's screen-content tools.
fn screen_content(width: u32, height: u32) -> Planes {
    planes(width, height, |x, y| {
        let cell = ((x / 8) + (y / 8)) % 4;
        [[16u8, 235, 128, 64][cell as usize]; 3]
    })
}

/// Encodes with libaom, then asserts gamut parses the whole header layer and agrees with libaom's
/// own decoder about the frame.
fn check(width: u32, height: u32, qindex: u8, content: &str, make: fn(u32, u32) -> Planes) {
    let (y, u, v) = make(width, height);
    let stream = aom_oracle::encode_still_intra(width, height, &y, &u, &v, qindex)
        .unwrap_or_else(|e| panic!("libaom encode {width}x{height} q{qindex} {content}: {e}"));

    let label = format!("{width}x{height} q{qindex} {content}");
    let decoder = Av1Decoder::new();
    let info = decoder
        .inspect(&stream)
        .unwrap_or_else(|e| panic!("gamut inspect {label}: {e}"));

    // libaom's reference decoder is the authority on what this stream says.
    let reference =
        aom_oracle::decode_av1(&stream).unwrap_or_else(|e| panic!("libaom decode {label}: {e}"));

    assert_eq!(
        info.frame.upscaled_width, reference.width,
        "width disagrees with libaom for {label}"
    );
    assert_eq!(
        info.frame.frame_height, reference.height,
        "height disagrees with libaom for {label}"
    );
    assert_eq!(
        u32::from(info.sequence.color.bit_depth.bits()),
        u32::from(reference.bit_depth),
        "bit depth disagrees with libaom for {label}"
    );
    assert_eq!(
        info.sequence.color.subsampling,
        ChromaSubsampling::Cs444,
        "the oracle encodes 4:4:4 for {label}"
    );
    assert_eq!(info.sequence.seq_profile, 1, "4:4:4 requires profile 1");
    assert!(
        info.tile_count >= 1,
        "the tile group must carry at least one tile for {label}"
    );
    assert_eq!(
        info.tile_count,
        info.frame.tile_info.tile_cols * info.frame.tile_info.tile_rows,
        "tile group size must match the tile grid for {label}"
    );
    assert_eq!(
        info.frame.coded_lossless,
        qindex == 0,
        "CodedLossless must track the lossless request for {label}"
    );
}

#[test]
fn parses_libaom_stills_across_sizes() {
    // Odd and sub-superblock sizes exercise the partial-superblock and dimension-bit paths.
    for &(w, h) in &[
        (1u32, 1u32),
        (7, 5),
        (16, 16),
        (64, 64),
        (65, 65),
        (127, 129),
        (256, 192),
    ] {
        check(w, h, 60, "gradient", gradient);
    }
}

#[test]
fn parses_libaom_stills_across_quantizers() {
    // 0 is lossless; the rest span the four coefficient-CDF quantizer contexts (§8.3.2).
    for &q in &[0u8, 20, 60, 120, 200, 255] {
        check(96, 96, q, "gradient", gradient);
    }
}

#[test]
fn parses_libaom_stills_across_content() {
    for &(name, make) in &[
        ("flat", flat as fn(u32, u32) -> Planes),
        ("gradient", gradient),
        ("noise", noise),
        ("screen", screen_content),
    ] {
        check(128, 128, 80, name, make);
    }
}

#[test]
fn libaom_streams_exercise_the_general_sequence_header() {
    // libaom's all-intra usage does **not** set `still_picture`, and so does not take the
    // `reduced_still_picture_header` shortcut either: it emits the general §5.5.1 form, with
    // `timing_info_present_flag`, `initial_display_delay_present_flag` and an explicit operating
    // point list. `gamut-av1`'s encoder only ever emits the reduced form, so this is the half of
    // the sequence header its own round-trip suite cannot reach — which is the point of checking
    // against a different encoder. If libaom ever changes this, this test says so rather than
    // letting the coverage quietly narrow.
    let (y, u, v) = gradient(64, 64);
    let stream = aom_oracle::encode_still_intra(64, 64, &y, &u, &v, 60).unwrap();
    let info = Av1Decoder::new().inspect(&stream).unwrap();
    assert!(
        !info.sequence.reduced_still_picture_header,
        "libaom is expected to emit the general sequence header form"
    );
    assert!(!info.sequence.still_picture);
    // The general form still resolves an operating point, and level 0 is the lowest.
    assert!(info.sequence.seq_level_idx <= 31);
}

#[test]
fn a_lossless_libaom_still_is_coded_lossless() {
    let (y, u, v) = gradient(64, 64);
    let stream = aom_oracle::encode_still_intra(64, 64, &y, &u, &v, 0).unwrap();
    let info = Av1Decoder::new().inspect(&stream).unwrap();
    assert!(info.frame.coded_lossless);
    assert!(
        info.frame.all_lossless,
        "no superres, so AllLossless follows"
    );
    assert_eq!(info.frame.quant.base_q_idx, 0);
    // CodedLossless forces ONLY_4X4 and disables every in-loop filter (§5.9.11/.19/.20).
    assert_eq!(info.frame.tx_mode, gamut_av1::TxMode::Only4x4);
    assert_eq!(info.frame.loop_filter.level, [0; 4]);
    assert!(!info.frame.lr.uses_lr);
}

#[test]
fn decode_limits_refuse_an_oversized_frame_before_allocating() {
    let (y, u, v) = gradient(128, 128);
    let stream = aom_oracle::encode_still_intra(128, 128, &y, &u, &v, 60).unwrap();

    let tight = Av1Decoder::new().with_limits(DecodeLimits::new().with_max_width(64));
    assert_eq!(
        tight.inspect(&stream).unwrap_err().static_message(),
        Some("AV1 decode: frame dimensions exceed the configured limit")
    );

    // 128x128 8-bit 4:4:4 decodes to 3 * 16384 bytes, past a 1 KiB cap.
    let tight = Av1Decoder::new().with_limits(DecodeLimits::new().with_max_image_bytes(1024));
    assert_eq!(
        tight.inspect(&stream).unwrap_err().static_message(),
        Some("AV1 decode: decoded frame size exceeds the configured limit")
    );

    // The default limits accept the same stream, so the refusals above are the caps talking.
    Av1Decoder::new().inspect(&stream).unwrap();
}

#[test]
fn truncating_a_libaom_stream_is_always_an_error_never_a_panic() {
    let (y, u, v) = gradient(96, 96);
    let stream = aom_oracle::encode_still_intra(96, 96, &y, &u, &v, 60).unwrap();
    let decoder = Av1Decoder::new();
    for cut in 0..stream.len() {
        // Every truncation must be refused. The full stream is the only length that may reach
        // the tile body.
        let err = decoder
            .inspect(&stream[..cut])
            .err()
            .unwrap_or_else(|| panic!("truncation to {cut} bytes was accepted"));
        assert!(
            err.static_message().is_some(),
            "truncation to {cut} bytes produced an untyped error"
        );
    }
    decoder.inspect(&stream).expect("the intact stream parses");
}

#[test]
fn corrupting_a_libaom_stream_never_panics() {
    let (y, u, v) = gradient(64, 64);
    let stream = aom_oracle::encode_still_intra(64, 64, &y, &u, &v, 60).unwrap();
    let decoder = Av1Decoder::new();
    // Flip one bit at a time through the header region. Some corruptions still parse to a
    // different-but-valid header; what must never happen is a panic or an out-of-bounds read.
    for byte in 0..stream.len().min(48) {
        for bit in 0..8 {
            let mut corrupted = stream.clone();
            corrupted[byte] ^= 1 << bit;
            let _ = decoder.inspect(&corrupted);
        }
    }
}

#[test]
fn a_stream_with_no_sequence_header_is_refused() {
    let (y, u, v) = gradient(64, 64);
    let stream = aom_oracle::encode_still_intra(64, 64, &y, &u, &v, 60).unwrap();
    // Drop everything up to the first frame OBU by keeping only the trailing bytes; whatever the
    // walk makes of them, it must not claim a decodable still.
    let err = Av1Decoder::new()
        .inspect(&stream[stream.len() / 2..])
        .expect_err("a stream without a sequence header must be refused");
    assert!(err.static_message().is_some());
}
