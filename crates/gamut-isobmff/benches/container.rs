//! ISOBMFF (still-image) container serialization throughput benchmarks (issue #149).
//!
//! Intentionally tight: the [`IsoBmffImage::to_bytes`] box assembler — `ftyp`/`meta`/`mdat` layout with `iloc`
//! offset back-patching — measured at a small and a large payload so the fixed box overhead and the
//! payload copy are both visible. The counter reports payload bytes per second (the payload is
//! synthetic; the writer never parses it). Run with `cargo bench -p gamut-isobmff`.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_core::Cicp;
use gamut_isobmff::{ColourInformation, IsoBmffImage, Item, Property, PropertyKind};

fn main() {
    divan::main();
}

#[divan::bench(args = [4 * 1024, 256 * 1024])]
fn write_still(bencher: Bencher, payload: usize) {
    let img = IsoBmffImage::new(
        *b"avif",
        vec![*b"avif", *b"mif1", *b"miaf", *b"MA1A"],
        1,
        vec![
            Item::new(1, *b"av01")
                .with_properties(vec![
                    Property::new(
                        true,
                        PropertyKind::CodecConfiguration {
                            kind: *b"av1C",
                            data: vec![0x81, 0x00, 0x0c, 0x00],
                        },
                    ),
                    Property::new(
                        false,
                        PropertyKind::ImageSpatialExtents {
                            width: 1024,
                            height: 1024,
                        },
                    ),
                    Property::new(
                        false,
                        PropertyKind::PixelInformation {
                            bits_per_channel: vec![8, 8, 8],
                        },
                    ),
                    Property::new(
                        false,
                        PropertyKind::Colour(ColourInformation::Nclx(Cicp {
                            colour_primaries: 1,
                            transfer_characteristics: 13,
                            matrix_coefficients: 0,
                            full_range: true,
                        })),
                    ),
                ])
                .with_payload(vec![0xA5u8; payload]),
        ],
    );
    bencher
        .counter(BytesCount::new(payload))
        .bench_local(|| black_box(&img).to_bytes().unwrap());
}
