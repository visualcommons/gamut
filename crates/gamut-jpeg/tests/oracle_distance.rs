//! jpegli's distance quality control (issue #1243), against its two oracles: the DQT tables
//! `JpegEncoder::with_distance` writes equal the tables `jpegli_set_distance` builds at the same
//! distance (`tooling/jpegli-oracle`), and libjpeg-turbo decodes every such stream exactly as
//! gamut's own decoder does (`tooling/libjpeg-oracle`).

use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_jpeg::{
    ChromaSubsampling, JpegColorMode, JpegDecoder, JpegEncoder, Process, ProgressiveScript,
    jpegli_quality_to_distance,
};
use jpegli_oracle::TableSpace;

/// A smooth RGB gradient with a little texture (so every table entry is exercised by real AC).
fn rgb_source(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            out.push(((x * 255) / w.max(1)) as u8);
            out.push(((y * 255) / h.max(1)) as u8);
            out.push(((x * 7 + y * 13) % 256) as u8);
        }
    }
    out
}

/// The frame a gamut encode builds, with the jpegli oracle query that describes the same frame.
#[derive(Debug, Clone, Copy)]
enum Frame {
    Gray,
    Ycbcr(ChromaSubsampling),
    Xyb(ChromaSubsampling),
}

/// Every frame layout `with_distance` serves.
const FRAMES: [Frame; 7] = [
    Frame::Gray,
    Frame::Ycbcr(ChromaSubsampling::Cs444),
    Frame::Ycbcr(ChromaSubsampling::Cs422),
    Frame::Ycbcr(ChromaSubsampling::Cs420),
    Frame::Ycbcr(ChromaSubsampling::Cs440),
    Frame::Xyb(ChromaSubsampling::Cs420),
    Frame::Xyb(ChromaSubsampling::Cs444),
];

/// Y's (XYB: X's and Y's) sampling factors for `s`.
fn factors(s: ChromaSubsampling) -> (u8, u8) {
    match s {
        ChromaSubsampling::Cs444 => (1, 1),
        ChromaSubsampling::Cs422 => (2, 1),
        ChromaSubsampling::Cs420 => (2, 2),
        ChromaSubsampling::Cs440 => (1, 2),
        other => panic!("not a JPEG layout: {other:?}"),
    }
}

/// gamut's encode of a `w × h` source as `frame` at `distance` under `process`.
fn encode(frame: Frame, distance: f32, process: Process, w: u32, h: u32) -> Vec<u8> {
    let dims = Dimensions::new(w, h).unwrap();
    let enc = JpegEncoder::new()
        .with_distance(distance)
        .with_process(process);
    let mut out = Vec::new();
    match frame {
        Frame::Gray => {
            let rgb = rgb_source(w, h);
            let gray: Vec<u8> = rgb
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| p[0] / 2 + p[2] / 2)
                .collect();
            enc.encode_image(ImageRef::<Gray8>::new(&gray, dims).unwrap(), &mut out)
        }
        Frame::Ycbcr(s) | Frame::Xyb(s) => {
            let mode = if matches!(frame, Frame::Xyb(_)) {
                JpegColorMode::Xyb
            } else {
                JpegColorMode::Ycbcr
            };
            let rgb = rgb_source(w, h);
            enc.with_subsampling(s)
                .with_color_mode(mode)
                .encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut out)
        }
    }
    .expect("gamut encode");
    out
}

/// Each frame component's `(Tq, natural-order table)`, read from the stream's DQT and SOF
/// segments. Every table must be 8-bit (`Pq = 0`), as jpegli's `force_baseline` tables are.
fn component_tables(jpeg: &[u8]) -> Vec<(u8, [u16; 64])> {
    // T.81 Figure A.6: zig-zag index → natural index.
    const ZIGZAG: [usize; 64] = [
        0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27,
        20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
        58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
    ];
    let mut tables = [None; 4];
    let mut tqs = Vec::new();
    let mut pos = 2; // past SOI
    while jpeg[pos + 1] != 0xDA {
        let len = usize::from(u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]));
        let payload = &jpeg[pos + 4..pos + 2 + len];
        match jpeg[pos + 1] {
            0xDB => {
                for t in payload.as_chunks::<65>().0 {
                    assert_eq!(t[0] >> 4, 0, "an 8-bit table");
                    let mut table = [0u16; 64];
                    for (i, &v) in t[1..].iter().enumerate() {
                        table[ZIGZAG[i]] = u16::from(v);
                    }
                    tables[usize::from(t[0] & 0x0F)] = Some(table);
                }
            }
            // SOF0, SOF1, SOF2.
            0xC0..=0xC2 => {
                tqs = (0..usize::from(payload[5]))
                    .map(|i| payload[8 + i * 3])
                    .collect();
            }
            _ => {}
        }
        pos += 2 + len;
    }
    tqs.iter()
        .map(|&tq| (tq, tables[usize::from(tq)].expect("table defined")))
        .collect()
}

/// jpegli's `(Tq, table)` per component for `frame` at `distance`.
fn jpegli_tables(frame: Frame, distance: f32) -> Vec<(u8, [u16; 64])> {
    let (space, sampling) = match frame {
        Frame::Gray => (TableSpace::Gray, (1, 1)),
        Frame::Ycbcr(s) => (TableSpace::Ycbcr, factors(s)),
        Frame::Xyb(s) => (TableSpace::Xyb, factors(s)),
    };
    jpegli_oracle::distance_tables(space, sampling, distance)
        .expect("jpegli table query")
        .into_iter()
        .map(|c| (c.tq, c.table))
        .collect()
}

#[test]
fn quality_to_distance_is_jpeglis_for_every_quality() {
    for q in 0..=u8::MAX {
        assert_eq!(
            jpegli_quality_to_distance(q).to_bits(),
            jpegli_oracle::quality_to_distance(i32::from(q)).to_bits(),
            "quality {q}"
        );
    }
}

#[test]
fn distance_dqt_tables_equal_jpeglis() {
    // Both sides of every branch of jpegli's scaling: the linear range below 1.5, the knee, the
    // power law, the standard-table branch's piecewise breakpoints (0.1, 4.6, 6.4, 25), and every
    // distance `cjpegli -q` reaches.
    let mut distances = vec![
        0.0, 0.01, 0.05, 0.1, 0.11, 0.3, 0.5, 0.75, 1.0, 1.2, 1.49, 1.5, 1.51, 2.0, 2.5, 3.0, 4.0,
        4.6, 4.61, 5.0, 6.0, 6.4, 6.41, 8.0, 10.0, 12.5, 16.0, 20.0, 24.9, 25.0, 30.0, 60.0,
    ];
    distances.extend((1..=100).map(jpegli_quality_to_distance));
    for frame in FRAMES {
        for &d in &distances {
            let jpeg = encode(frame, d, Process::Baseline, 16, 16);
            assert_eq!(
                component_tables(&jpeg),
                jpegli_tables(frame, d),
                "{frame:?} at distance {d}"
            );
        }
    }
}

#[test]
fn libjpeg_turbo_decodes_every_distance_encode_as_gamut_does() {
    let (w, h) = (37u32, 29u32);
    let processes = [
        Process::Baseline,
        Process::Optimized,
        Process::Progressive(ProgressiveScript::Libjpeg),
        Process::Progressive(ProgressiveScript::Mozjpeg),
    ];
    let decoder = JpegDecoder::new();
    for frame in FRAMES {
        for d in [0.0, 1.0, 3.0, 25.0] {
            for process in processes {
                let jpeg = encode(frame, d, process, w, h);
                let theirs = libjpeg_oracle::decode(&jpeg).unwrap_or_else(|e| {
                    panic!("libjpeg-turbo rejected {frame:?} d={d} {process:?}: {e}")
                });
                assert_eq!((theirs.width, theirs.height), (w, h));
                let ours = if let Frame::Gray = frame {
                    let img: ImageBuf<Gray8> = decoder.decode_image(&jpeg).unwrap();
                    img.as_samples().to_vec()
                } else {
                    let img: ImageBuf<Rgb8> = decoder.decode_image(&jpeg).unwrap();
                    img.as_samples().to_vec()
                };
                assert_eq!(
                    theirs.pixels, ours,
                    "{frame:?} d={d} {process:?}: libjpeg-turbo and gamut decode differently"
                );
            }
        }
    }
}
