//! The tier-3 conformance gate at the YUV-plane level, against libwebp, in both directions:
//!   - **gamut encode → libwebp decode == gamut decode**, across every encoder feature (prediction,
//!     loop filters, segmentation, token partitions, loop-filter deltas, skip, every effort rung) —
//!     pinning gamut's streams as conformant;
//!   - **libwebp encode → gamut decode == libwebp decode**, bit-exact — pinning gamut's decoder
//!     against the full feature surface a production encoder emits (per-segment filter levels,
//!     probability updates, …).
//!
//! The planes are compared rather than RGB because the encoder's RGB→YCbCr is
//! implementation-defined and off the bit-exact gate, while a VP8 bitstream decodes to the same
//! integer YUV in any conformant decoder. It drives the frame coder directly — quantizer indices,
//! the simple filter, segmentation, partitions and loop-filter deltas are not reachable through
//! `WebpEncoder` — so it lives here rather than in `tests/oracle.rs` (issue #1163).

use gamut_riff::write_simple_lossy;
use libwebp_oracle::fixtures::{LARGE_DIMENSIONS, pattern_rgba, photo_like_rgba};
use libwebp_oracle::{
    LibwebpLossyConfig, libwebp_decode_yuv, libwebp_encode_lossy_rgba,
    libwebp_encode_lossy_rgba_config,
};

use super::fixtures::{detailed, photo_like, synthetic};
use super::{EncodeOptions, LoopFilterDeltas, decode_frame, encode_frame, encode_frame_filtered};
use crate::Effort;

/// Requires gamut's decode of the VP8 `payload` and libwebp's decode of the WebP file `webp`
/// carrying it to be a `w`×`h` frame with bit-identical Y, U and V planes.
fn assert_yuv_matches_libwebp(payload: &[u8], webp: &[u8], (w, h): (u32, u32), label: &str) {
    let lib = libwebp_decode_yuv(webp);
    let gamut = decode_frame(payload)
        .expect("gamut decode")
        .to_yuv420()
        .expect("crop");
    assert_eq!((lib.width, lib.height), (w, h), "dims {label}");
    assert_eq!(gamut.planes()[0], lib.y.as_slice(), "Y {label}");
    assert_eq!(gamut.planes()[1], lib.u.as_slice(), "U {label}");
    assert_eq!(gamut.planes()[2], lib.v.as_slice(), "V {label}");
}

/// The forward direction: wraps a gamut-encoded VP8 `payload` in a simple lossy file and
/// requires libwebp to decode it to exactly what gamut's own decoder produces.
fn assert_libwebp_decodes_like_gamut(payload: &[u8], dims: (u32, u32), label: &str) {
    let webp = write_simple_lossy(payload).unwrap();
    assert_yuv_matches_libwebp(payload, &webp, dims, label);
}

/// The reverse direction: requires gamut's decode of the `VP8 ` chunk of a libwebp-encoded
/// file to match libwebp's own decode of it.
fn assert_gamut_decodes_like_libwebp(webp: &[u8], dims: (u32, u32), label: &str) {
    assert_yuv_matches_libwebp(&vp8_payload(webp), webp, dims, label);
}

/// Extracts the `VP8 ` (lossy) chunk payload from a RIFF/WebP file.
fn vp8_payload(webp: &[u8]) -> Vec<u8> {
    use gamut_riff::{RiffReader, WebpChunkId};
    RiffReader::new(webp)
        .expect("riff")
        .filter_map(Result::ok)
        .find(|c| matches!(WebpChunkId::from(c.fourcc), WebpChunkId::Vp8))
        .expect("VP8 chunk")
        .payload
        .to_vec()
}

#[test]
fn gamut_lossy_bpred_matches_libwebp_bit_exact() {
    // Detailed content drives gamut's encoder into per-4×4 B_PRED macroblocks; libwebp must
    // decode the same gamut bitstream to identical YUV — the tier-3 conformance gate for the
    // B_PRED path.
    for &(w, h) in &[(16u32, 16u32), (32, 32), (48, 48), (49, 33), (64, 16)] {
        for &quant_index in &[0u8, 8, 40] {
            let (payload, _) = encode_frame(&detailed(w, h), quant_index)
                .expect("fixture fits the partition-size fields");
            assert_libwebp_decodes_like_gamut(
                &payload,
                (w, h),
                &format!("B_PRED at {w}x{h} q{quant_index}"),
            );
        }
    }
}

#[test]
fn gamut_lossy_options_match_libwebp_bit_exact() {
    // The alternative encoder paths — the simple loop filter, quantizer segmentation and token
    // partitions — must each produce a stream libwebp decodes identically to gamut's decoder.
    // libwebp's own encoder always writes one token partition, so this forward direction is
    // the only one that pins gamut's multi-partition decode against the reference.
    let base = EncodeOptions::default();
    let cases = [
        (
            "simple-filter",
            EncodeOptions {
                simple_filter: true,
                ..base
            },
        ),
        (
            "segmented",
            EncodeOptions {
                segmented: true,
                ..base
            },
        ),
        (
            "segmented+simple",
            EncodeOptions {
                simple_filter: true,
                segmented: true,
                ..base
            },
        ),
        (
            "partitions-2",
            EncodeOptions {
                partitions: 2,
                ..base
            },
        ),
        (
            "partitions-4",
            EncodeOptions {
                partitions: 4,
                ..base
            },
        ),
        (
            "partitions-8",
            EncodeOptions {
                partitions: 8,
                ..base
            },
        ),
        (
            "everything",
            EncodeOptions {
                simple_filter: true,
                segmented: true,
                partitions: 4,
                ..base
            },
        ),
    ];
    for (label, opts) in cases {
        // (33, 145) spans ten macroblock rows, so the eight-partition cases route across every
        // one.
        for &(w, h) in &[(32u32, 32u32), (48, 48), (49, 33), (33, 145)] {
            for &q in &[12u8, 48] {
                let (payload, _) = encode_frame_filtered(&detailed(w, h), q, opts)
                    .expect("fixture fits the partition-size fields");
                assert_libwebp_decodes_like_gamut(
                    &payload,
                    (w, h),
                    &format!("{label} at {w}x{h} q{q}"),
                );
            }
        }
    }
}

#[test]
fn gamut_lossy_yuv_matches_libwebp_bit_exact() {
    // A VP8 bitstream decodes to a deterministic integer YUV, so gamut's own decoder and
    // libwebp must agree bit-for-bit on the same gamut-produced bitstream — the tier-3
    // conformance gate that pins the encoder to a spec-valid stream. (`tests` above
    // additionally checks encoder-recon == its own decoder; together these need no pixel
    // tolerance.)
    for &(w, h) in &[
        (16u32, 16u32),
        (32, 32),
        (17, 9),
        (64, 48),
        (80, 16),
        (33, 49),
    ] {
        for &quant_index in &[0u8, 20, 60, 110] {
            let (payload, _) = encode_frame(&synthetic(w, h), quant_index)
                .expect("fixture fits the partition-size fields");
            assert_libwebp_decodes_like_gamut(
                &payload,
                (w, h),
                &format!("at {w}x{h} q{quant_index}"),
            );
        }
    }
}

#[test]
fn gamut_lossy_yuv_realistic_and_large_matches_libwebp() {
    // The same gate on photographic-like YUV and on canvases far larger than the small
    // synthetic frames: macroblock grids spanning dozens of rows and realistic residual/token
    // distributions. Large sizes span many MB rows (768/16 = 48) and an awkward width
    // (513×97); gamut's encoder is fast enough here that the full range stays cheap.
    let dims = [
        (32u32, 32u32),
        (64, 48),
        (256, 256),
        (384, 288),
        (640, 480),
        (1024, 768),
        (513, 97),
    ];
    for &(w, h) in &dims {
        for &quant_index in &[12u8, 56] {
            let (payload, _) = encode_frame(&photo_like(w, h, 0x7e57), quant_index)
                .expect("fixture fits the partition-size fields");
            assert_libwebp_decodes_like_gamut(
                &payload,
                (w, h),
                &format!("at {w}x{h} q{quant_index}"),
            );
        }
    }
}

#[test]
fn gamut_lossy_loop_filter_deltas_match_libwebp_bit_exact() {
    // mb_lf_adjustments (RFC 6386 §9.4) are a decode path libwebp supports but cwebp never
    // emits, so the only way to pin gamut's *application* of them against the reference is to
    // emit them from gamut's encoder and require libwebp to decode the result identically to
    // gamut's own decoder. (An internal encode→decode round-trip can't: the encoder and decoder
    // share `apply_loop_filter`, so a bug there would cancel.) `detailed` forces B_PRED
    // macroblocks, exercising the mode[0] (B_PRED) delta alongside ref_frame[0].
    let cases = [
        LoopFilterDeltas {
            ref_frame: [12, 0, 0, 0],
            mode: [0; 4],
        }, // intra ref-frame delta only
        LoopFilterDeltas {
            ref_frame: [0; 4],
            mode: [10, 0, 0, 0],
        }, // B_PRED mode delta only
        LoopFilterDeltas {
            ref_frame: [-20, 0, 0, 0],
            mode: [0; 4],
        }, // negative: clamps levels toward 0
        LoopFilterDeltas {
            ref_frame: [8, 0, 0, 0],
            mode: [-12, 0, 0, 0],
        },
    ];

    // Guard: the deltas must actually change libwebp's decoded output, else a silently-dropped
    // delta would make the conformance assertions below vacuous.
    {
        let yuv = detailed(48, 48);
        let base = write_simple_lossy(
            &encode_frame(&yuv, 16)
                .expect("fixture fits the partition-size fields")
                .0,
        )
        .unwrap();
        let with = write_simple_lossy(
            &encode_frame_filtered(
                &yuv,
                16,
                EncodeOptions {
                    loop_filter_deltas: cases[0],
                    ..Default::default()
                },
            )
            .expect("fixture fits the partition-size fields")
            .0,
        )
        .unwrap();
        assert_ne!(
            libwebp_decode_yuv(&base).y,
            libwebp_decode_yuv(&with).y,
            "loop-filter deltas must change libwebp's decoded luma"
        );
    }

    for deltas in cases {
        for &(w, h) in &[(32u32, 32u32), (48, 48), (49, 33)] {
            for &q in &[16u8, 48] {
                let opts = EncodeOptions {
                    loop_filter_deltas: deltas,
                    ..Default::default()
                };
                let (payload, _) = encode_frame_filtered(&detailed(w, h), q, opts)
                    .expect("fixture fits the partition-size fields");
                assert_libwebp_decodes_like_gamut(
                    &payload,
                    (w, h),
                    &format!("{deltas:?} at {w}x{h} q{q}"),
                );
            }
        }
    }
}

#[test]
fn gamut_decodes_patched_vp8_profiles_like_libwebp() {
    // VP8 profiles 1–3 (the 3-bit frame-tag version) are a decode path cwebp never emits.
    // Patch the version field (bits 1–3 of byte 0) of a gamut key frame and require gamut and
    // libwebp to decode it identically — pinning that the profile field does not alter intra
    // key-frame reconstruction (the explicit filter-type bit governs), matching the reference
    // rather than the RFC's prose.
    for &(w, h) in &[(32u32, 32u32), (49, 33)] {
        let (payload, _) =
            encode_frame(&detailed(w, h), 24).expect("fixture fits the partition-size fields");
        for version in 1u8..=3 {
            let mut patched = payload.clone();
            patched[0] = (patched[0] & !0b1110) | (version << 1);
            assert_libwebp_decodes_like_gamut(&patched, (w, h), &format!("v{version} at {w}x{h}"));
        }
    }
}

#[test]
fn gamut_decodes_libwebp_lossy_bit_exact() {
    // The reverse-direction conformance gate: a real libwebp lossy encoder emits VP8 streams
    // using its own loop filter, segmentation, token-probability updates, and skip choices.
    // gamut's native decoder must reproduce libwebp's own YUV output bit-for-bit — proving it
    // handles the full feature surface a production encoder actually emits, not just gamut's
    // own streams.
    for &(w, h) in &[
        (16u32, 16u32),
        (32, 32),
        (64, 48),
        (49, 33),
        (80, 17),
        (255, 3),
    ] {
        for q in [6.0f32, 35.0, 70.0, 100.0] {
            let webp = libwebp_encode_lossy_rgba(&pattern_rgba(w, h), w, h, q);
            assert_gamut_decodes_like_libwebp(&webp, (w, h), &format!("at {w}x{h} q{q}"));
        }
    }
}

#[test]
fn gamut_decodes_libwebp_lossy_realistic_and_large() {
    // The reverse direction at scale: libwebp lossy-encodes photographic RGBA over a
    // lossy-appropriate small matrix plus the full LARGE_DIMENSIONS (up to 1024×768), and
    // gamut's decoder must reproduce libwebp's own YUV bit-for-bit — pinning the decode paths
    // (per-segment filter levels, probability updates, partitioning) on frames far larger and
    // more varied than the tiny synthetic inputs.
    let small = [
        (16u32, 16u32),
        (32, 32),
        (64, 48),
        (49, 33),
        (80, 17),
        (255, 3),
    ];
    for &(w, h) in small.iter().chain(LARGE_DIMENSIONS) {
        for q in [20.0f32, 80.0] {
            let webp = libwebp_encode_lossy_rgba(&photo_like_rgba(w, h, 0x1d0f), w, h, q);
            assert_gamut_decodes_like_libwebp(&webp, (w, h), &format!("at {w}x{h} q{q}"));
        }
    }
}

#[test]
fn gamut_decodes_libwebp_lossy_forced_features_bit_exact() {
    // The one-shot `WebPEncodeRGBA` the other reverse-direction tests use can't independently
    // reach the VP8 feature surface — it runs at a fixed method with the complex loop filter
    // and libwebp's default segmentation. Drive libwebp's *advanced* encoder to force each knob
    // in turn (simple vs complex filter, 1..=4 segments, low/high effort, filter strength, then
    // several at once) and require gamut's decoder to reproduce libwebp's own YUV bit-for-bit
    // on every variant — pinning the decoder against streams a production encoder can emit but
    // cwebp's defaults rarely do.
    let base = LibwebpLossyConfig {
        quality: 75.0,
        filter_type: 1,
        segments: 1,
        method: 4,
        filter_strength: 60,
    };

    // Guard: prove the advanced config actually reaches the encoder. This is a *conformance*
    // gate (gamut must match whatever libwebp emits), so it would still pass if the forcing
    // were silently ignored — but then it would add nothing over the default-config reverse
    // oracle. Require distinct feature settings to produce distinct streams for the same input.
    // (Token-partition count is *not* guarded here: libwebp's encoder ignores
    // `config.partitions` and always writes one partition — see `LibwebpLossyConfig` — so that
    // path is covered forward in `gamut_lossy_options_*` instead.)
    {
        let (w, h) = (64u32, 48);
        let rgba = photo_like_rgba(w, h, 0x00c0_ffee);
        let one = libwebp_encode_lossy_rgba_config(&rgba, w, h, &base);
        let simple = libwebp_encode_lossy_rgba_config(
            &rgba,
            w,
            h,
            &LibwebpLossyConfig {
                filter_type: 0,
                ..base
            },
        );
        assert_ne!(
            one, simple,
            "switching to the simple loop filter must change the stream"
        );
        let multi_seg = libwebp_encode_lossy_rgba_config(
            &rgba,
            w,
            h,
            &LibwebpLossyConfig {
                segments: 4,
                ..base
            },
        );
        assert_ne!(
            one, multi_seg,
            "forcing four segments must change the stream"
        );
    }

    let mut cases: Vec<(String, LibwebpLossyConfig)> = Vec::new();
    for filter_type in [0, 1] {
        cases.push((
            format!("filter_type={filter_type}"),
            LibwebpLossyConfig {
                filter_type,
                ..base
            },
        ));
    }
    for segments in [1, 2, 3, 4] {
        cases.push((
            format!("segments={segments}"),
            LibwebpLossyConfig { segments, ..base },
        ));
    }
    for method in [0, 3, 6] {
        cases.push((
            format!("method={method}"),
            LibwebpLossyConfig { method, ..base },
        ));
    }
    for filter_strength in [0, 30] {
        cases.push((
            format!("filter_strength={filter_strength}"),
            LibwebpLossyConfig {
                filter_strength,
                ..base
            },
        ));
    }
    cases.push((
        "combined".into(),
        LibwebpLossyConfig {
            filter_type: 0,
            segments: 4,
            method: 6,
            filter_strength: 20,
            ..base
        },
    ));

    // (128, 96) spans six MB rows, exercising per-segment filter levels across many rows.
    for &(w, h) in &[(32u32, 32u32), (64, 48), (49, 33), (128, 96)] {
        let rgba = photo_like_rgba(w, h, 0x00c0_ffee);
        for (label, cfg) in &cases {
            let webp = libwebp_encode_lossy_rgba_config(&rgba, w, h, cfg);
            assert_gamut_decodes_like_libwebp(&webp, (w, h), &format!("{label} at {w}x{h}"));
        }
    }
}

#[test]
fn libwebp_decodes_every_effort_level_bit_exactly() {
    // The ladder's conformance gate. Each rung changes what the encoder emits — 4x4 search on
    // or off, derived coefficient probabilities, a measured skip probability, a dead-zone
    // quantizer — and every one of those must still produce a stream libwebp reconstructs
    // identically to gamut's own decoder. Without this, a rung could quietly ship a stream only
    // gamut can read.
    //
    // Swept over efforts x sizes x quantizers rather than crossed with the full dimension
    // matrices the other tests use: each rung needs conformance coverage, not conformance
    // coverage of every shape, and the combinatorial version would dominate the suite's
    // runtime.
    for level in 0..=6u8 {
        let effort = Effort::from_level(level).expect("0..=6");
        let opts = EncodeOptions {
            effort,
            ..EncodeOptions::default()
        };
        for &(w, h) in &[(32u32, 32u32), (49, 33)] {
            for &q in &[12u8, 48] {
                for (kind, yuv) in [("detailed", detailed(w, h)), ("photo", photo_like(w, h, 7))] {
                    let (payload, _) = encode_frame_filtered(&yuv, q, opts)
                        .expect("fixture fits the partition-size fields");
                    assert_libwebp_decodes_like_gamut(
                        &payload,
                        (w, h),
                        &format!("effort {level} {kind} {w}x{h} q{q}"),
                    );
                }
            }
        }
    }
}
