//! integration · exact-value — the identifier contracts: `codec_id` is big-endian, and the
//! `pixel_format` namespace's three ranges stay disjoint.

use gamut_codec_abi::{codec_id, pixel_format};

#[test]
fn codec_id_puts_the_first_fourcc_byte_most_significant() {
    assert_eq!(codec_id(*b"av01"), 0x6176_3031);
    assert_eq!(codec_id(*b"hvc1").to_be_bytes(), *b"hvc1");
}

#[test]
fn fourcc_pixel_formats_are_their_big_endian_codes() {
    assert_eq!(pixel_format::I420, 0x4934_3230);
    assert_eq!(pixel_format::BGRA.to_be_bytes(), *b"BGRA");
}

#[test]
fn pixel_format_ranges_are_disjoint() {
    let channel_layouts = [
        pixel_format::GRAY8,
        pixel_format::BILEVEL,
        pixel_format::INDEXED8,
        pixel_format::RGB8,
        pixel_format::RGBA8,
        pixel_format::CMYK8,
        pixel_format::GRAY_ALPHA8,
        pixel_format::GRAY16,
        pixel_format::RGB16,
        pixel_format::RGBA16,
        pixel_format::GRAY_ALPHA16,
    ];
    let fourcc = [
        pixel_format::Y800,
        pixel_format::I420,
        pixel_format::I422,
        pixel_format::I444,
        pixel_format::BGRA,
    ];
    // The channel-layout range mirrors `gamut_core::PixelFormat`'s discriminants, which run 0, 1, ….
    for (discriminant, tag) in channel_layouts.into_iter().enumerate() {
        assert_eq!(tag as usize, discriminant);
    }
    for tag in fourcc {
        assert!(
            tag > 0xFFFF,
            "{tag:#x} collides with the channel-layout range"
        );
        assert!(tag.to_be_bytes().iter().all(u8::is_ascii_graphic));
    }
    assert!(
        !pixel_format::OPAQUE_BYTES
            .to_be_bytes()
            .iter()
            .all(u8::is_ascii_graphic)
    );
}
