//! integration · example — the descriptors' plain-data contract: each constructor initialises
//! every field, and `extradata` borrows exactly what was attached.
//!
//! The `struct_size` field these constructors also set is a forward-compatibility guard, and it is
//! pinned in `abi_guards.rs` instead; here `struct_size` is only one more field to initialise.

mod common;

use common::{TEST_CODEC, one_plane};
use gamut_codec_abi::{EncodeConfig, StreamConfig};

#[test]
fn stream_config_new_initialises_every_field() {
    let cfg = StreamConfig::new(TEST_CODEC, 640, 480, 10);

    assert_eq!(cfg.codec_id, TEST_CODEC);
    assert_eq!(cfg.width, 640);
    assert_eq!(cfg.height, 480);
    assert_eq!(cfg.bit_depth, 10);
    // No extradata is attached by the constructor, so the pair reads as "absent".
    assert_eq!(cfg.extradata_len, 0);
    assert!(cfg.extradata.is_null());
}

#[test]
fn encode_config_new_initialises_every_field() {
    let cfg = EncodeConfig::new(TEST_CODEC, 80);

    assert_eq!(cfg.codec_id, TEST_CODEC);
    assert_eq!(cfg.quality, 80);
    assert_eq!(cfg.extra_len, 0);
    assert!(cfg.extra.is_null());
}

#[test]
fn image_desc_new_initialises_every_field() {
    let mut buf = [0u8; 8];
    let desc = one_plane(4, 2, buf.as_mut_ptr(), 4);

    assert_eq!(desc.pixel_format, 0);
    assert_eq!(desc.width, 4);
    assert_eq!(desc.height, 2);
    assert_eq!(desc.bit_depth, 8);
    assert_eq!(desc.plane_count, 1);
    assert_eq!(desc.strides[0], 4);
    // Planes past `plane_count` stay null rather than carrying a stale pointer.
    assert!(desc.planes[1].is_null());
}

#[test]
fn stream_config_extradata_is_empty_when_none_is_attached() {
    let cfg = StreamConfig::new(TEST_CODEC, 1, 1, 8);

    // SAFETY: no extradata is attached, so this takes the empty-slice path and never
    // dereferences the null pointer.
    assert_eq!(unsafe { cfg.extradata() }, &[] as &[u8]);
}

#[test]
fn stream_config_extradata_borrows_the_attached_record() {
    let record = [0xAAu8, 0xBB, 0xCC];
    let mut cfg = StreamConfig::new(TEST_CODEC, 1, 1, 8);
    cfg.extradata = record.as_ptr();
    cfg.extradata_len = record.len();

    // SAFETY: `record` outlives the borrow and covers `extradata_len` bytes.
    assert_eq!(unsafe { cfg.extradata() }, &[0xAA, 0xBB, 0xCC]);
}

#[test]
fn stream_config_extradata_honours_a_length_shorter_than_the_record() {
    let record = [0xAAu8, 0xBB, 0xCC];
    let mut cfg = StreamConfig::new(TEST_CODEC, 1, 1, 8);
    cfg.extradata = record.as_ptr();
    cfg.extradata_len = 2;

    // The length field, not the backing allocation, decides how much is borrowed.
    // SAFETY: `record` outlives the borrow and covers the 2 bytes claimed.
    assert_eq!(unsafe { cfg.extradata() }, &[0xAA, 0xBB]);
}
