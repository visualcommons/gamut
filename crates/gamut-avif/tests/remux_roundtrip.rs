//! Hermetic remux round-trip: `gamut-isobmff` parses a *foreign* (libavif-encoded) AVIF, re-writes
//! it normalised, and libavif decodes the re-muxed container to identical pixels.
//!
//! This guards a guarantee `gamut-isobmff`'s own `read(&write) == model` check cannot: that
//! re-serialising a container this crate did **not** write preserves everything that affects
//! rendering (the coded payload is carried verbatim through the box model). libavif (dav1d backend)
//! is linked from the `third_party/libavif` + `third_party/dav1d` submodules via the
//! `libavif-oracle` dev-dependency, so — like `decode_roundtrip.rs` — the check is hermetic but
//! needs cmake/meson/ninja/nasm and the checked-out submodules.

use std::path::PathBuf;

use gamut_isobmff::TopLevelBox;

/// C2PA 2.4 §A.5.1: the `ContentProvenanceBox` user type `D8FEC3D6-1B0E-483C-9297-5828877EC481`.
const C2PA_UUID: [u8; 16] = [
    0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
];

/// A real 4:4:4 libavif corpus file. The oracle's `decode_avif` only handles 4:4:4 (the form gamut
/// emits), so a 4:2:0 fixture would be rejected before the pixel comparison.
fn corpus_444() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/libavif/tests/data/io/cosmos1650_yuv444_10bpc_p3pq.avif")
}

#[test]
fn remux_preserves_decoded_pixels() {
    let src = std::fs::read(corpus_444()).expect("read the corpus fixture");

    // gamut-isobmff parses the foreign container and re-serialises it (normalised box versions,
    // single-extent mdat), never touching the opaque coded payload.
    let model = gamut_isobmff::read(&src).expect("gamut-isobmff reads the foreign container");
    let remuxed = gamut_isobmff::write(&model).expect("gamut-isobmff writes the container");
    assert_ne!(
        remuxed, src,
        "the writer normalises, so the container bytes are expected to differ"
    );

    // The reference decoder must reproduce identical pixels from both containers.
    let original = libavif_oracle::decode_avif(&src).expect("libavif decodes the original");
    let round_tripped =
        libavif_oracle::decode_avif(&remuxed).expect("libavif decodes the re-muxed container");

    assert_eq!(
        (original.width, original.height, original.bit_depth),
        (
            round_tripped.width,
            round_tripped.height,
            round_tripped.bit_depth
        ),
    );
    assert_eq!(
        original.planes, round_tripped.planes,
        "decoded pixels differ after remux — the container round-trip lost rendering data"
    );
}

#[test]
fn remux_with_a_c2pa_uuid_box_preserves_decoded_pixels() {
    // A top-level C2PA `ContentProvenanceBox` — a `uuid` box with the §A.5.1 user type, placed
    // after `ftyp` and before the first `mdat` per §A.5.3 (`TopLevelPosition::AfterFtyp`) — must be
    // invisible to a conforming reader: libavif decodes the container carrying it to exactly the
    // pixels it decodes from the original. The payload is opaque bytes standing in for a manifest
    // store; nothing here validates it. Where the box lands is pinned exact-byte in
    // gamut-isobmff's `tests/top_level.rs`, not here.
    let src = std::fs::read(corpus_444()).expect("read the corpus fixture");
    let mut model = gamut_isobmff::read(&src).expect("gamut-isobmff reads the foreign container");
    model.push_top_level_box(TopLevelBox::uuid(
        C2PA_UUID,
        b"opaque-manifest-store".to_vec(),
    ));
    let with_box = gamut_isobmff::write(&model).expect("gamut-isobmff writes the container");

    let original = libavif_oracle::decode_avif(&src).expect("libavif decodes the original");
    let carrying = libavif_oracle::decode_avif(&with_box)
        .expect("libavif decodes the container carrying a C2PA uuid box");
    assert_eq!(
        (original.width, original.height, original.bit_depth),
        (carrying.width, carrying.height, carrying.bit_depth),
    );
    assert_eq!(
        original.planes, carrying.planes,
        "decoded pixels differ once a top-level uuid box is carried"
    );
}
