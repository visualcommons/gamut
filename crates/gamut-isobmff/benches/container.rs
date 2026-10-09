//! ISOBMFF (still-image) container serialization throughput benchmarks (issue #149).
//!
//! Intentionally tight: the [`write`] box assembler — `ftyp`/`meta`/`mdat` layout with `iloc`
//! offset back-patching — measured at a small and a large payload so the fixed box overhead and the
//! payload copy are both visible. The counter reports payload bytes per second (the payload is
//! synthetic; the writer never parses it). Run with `cargo bench -p gamut-isobmff`.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_isobmff::{
    ColourInformation, IsoBmffImage, Item, NclxColr, Property, PropertyKind, write,
};

fn main() {
    divan::main();
}

#[divan::bench(args = [4 * 1024, 256 * 1024])]
fn write_still(bencher: Bencher, payload: usize) {
    let img = IsoBmffImage::new(
        *b"avif",
        vec![*b"avif", *b"mif1", *b"miaf", *b"MA1A"],
        1,
        vec![Item {
            id: 1,
            item_type: *b"av01",
            name: String::new(),
            content_type: None,
            content_encoding: None,
            hidden: false,
            references: vec![],
            properties: vec![
                Property {
                    essential: true,
                    kind: PropertyKind::CodecConfiguration {
                        kind: *b"av1C",
                        data: vec![0x81, 0x00, 0x0c, 0x00],
                    },
                },
                Property {
                    essential: false,
                    kind: PropertyKind::ImageSpatialExtents {
                        width: 1024,
                        height: 1024,
                    },
                },
                Property {
                    essential: false,
                    kind: PropertyKind::PixelInformation {
                        bits_per_channel: vec![8, 8, 8],
                    },
                },
                Property {
                    essential: false,
                    kind: PropertyKind::Colour(ColourInformation::Nclx(NclxColr {
                        colour_primaries: 1,
                        transfer_characteristics: 13,
                        matrix_coefficients: 0,
                        full_range: true,
                    })),
                },
            ],
            payload: vec![0xA5u8; payload],
        }],
    );
    bencher
        .counter(BytesCount::new(payload))
        .bench_local(|| write(black_box(&img)).unwrap());
}
