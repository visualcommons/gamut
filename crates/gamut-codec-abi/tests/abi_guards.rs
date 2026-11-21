//! integration · example — the two forward-compatibility guards: the `struct_size` prefix leading
//! every descriptor, and `abi_version` leading every vtable.
//!
//! A descriptor built against another revision of this crate is read for the prefix its
//! `struct_size` covers — an older, shorter one for the fields it wrote, a newer, longer one for
//! the fields this build knows — and refused only when it is too short for a field the reader
//! needs. A vtable built against another revision is *refused*. Each guard is pinned at its
//! boundary: at compile time where the helpers are `const`, and at run time through the bridge
//! shims that apply them to foreign bytes.

mod common;

use core::cell::Cell;
use core::ffi::c_void;
use core::ptr;
use std::rc::Rc;

use common::{REPORTED_CICP, TEST_CODEC, counted_decoder, one_plane};
use gamut_codec_abi::bridge::{
    ForeignDecoder, ForeignEncoder, Lowered, lower_decoder, lower_encoder,
};
use gamut_codec_abi::{
    ABI_VERSION, Decoder, DecoderVTable, EncodeConfig, Encoder, EncoderVTable, ImageDesc,
    MAX_PLANES, Status, StreamConfig,
};

/// An [`ImageDesc`] with no planes, usable in `const` context.
const fn bare_desc() -> ImageDesc {
    ImageDesc::new(
        0,
        4,
        2,
        8,
        1,
        [ptr::null_mut(); MAX_PLANES],
        [0; MAX_PLANES],
    )
}

/// `desc` as a producer built with a `struct_size` of `size` wrote it.
const fn sized(mut desc: ImageDesc, size: usize) -> ImageDesc {
    desc.struct_size = size;
    desc
}

/// `desc`'s colour group, in [`ImageDesc::UNSIGNALLED`]'s shape.
fn cicp(desc: &ImageDesc) -> (u16, u16, u16, bool) {
    (
        desc.color_primaries,
        desc.transfer_characteristics,
        desc.matrix_coefficients,
        desc.is_full_range(),
    )
}

// The compile-time half of the prefix rule: a current reader accepts an older `ImageDesc` that
// stops at the mandatory prefix — and reads no colour group from it — and refuses one a byte too
// short for the last mandatory field.
const _: () = {
    let older = sized(bare_desc(), ImageDesc::MIN_SIZE);
    assert!(older.is_compatible());
    assert!(!older.has_cicp());
    assert!(!sized(bare_desc(), ImageDesc::MIN_SIZE - 1).is_compatible());
    assert!(sized(bare_desc(), ImageDesc::CICP_END).has_cicp());
    assert!(!sized(bare_desc(), ImageDesc::CICP_END - 1).has_cicp());
};

#[test]
fn stream_config_new_writes_the_compiled_layout_size() {
    let cfg = StreamConfig::new(TEST_CODEC, 640, 480, 10);

    assert_eq!(cfg.struct_size, size_of::<StreamConfig>());
    assert!(cfg.is_abi_current());
    assert!(cfg.is_compatible());
}

#[test]
fn stream_config_accepts_exactly_the_prefixes_that_reach_its_last_mandatory_field() {
    let mut cfg = StreamConfig::new(TEST_CODEC, 640, 480, 10);

    cfg.struct_size = StreamConfig::MIN_SIZE;
    assert!(cfg.is_compatible());
    cfg.struct_size = StreamConfig::MIN_SIZE - 1;
    assert!(!cfg.is_compatible());
    // A newer, longer producer is read for the fields this build knows.
    cfg.struct_size = size_of::<StreamConfig>() + 8;
    assert!(cfg.is_compatible());
}

#[test]
fn encode_config_new_writes_the_compiled_layout_size() {
    let cfg = EncodeConfig::new(TEST_CODEC, 80);

    assert_eq!(cfg.struct_size, size_of::<EncodeConfig>());
    assert!(cfg.is_abi_current());
    assert!(cfg.is_compatible());
}

#[test]
fn encode_config_accepts_exactly_the_prefixes_that_reach_its_last_mandatory_field() {
    let mut cfg = EncodeConfig::new(TEST_CODEC, 80);

    cfg.struct_size = EncodeConfig::MIN_SIZE;
    assert!(cfg.is_compatible());
    cfg.struct_size = EncodeConfig::MIN_SIZE - 1;
    assert!(!cfg.is_compatible());
    cfg.struct_size = size_of::<EncodeConfig>() + 8;
    assert!(cfg.is_compatible());
}

#[test]
fn image_desc_new_writes_the_compiled_layout_size() {
    let mut buf = [0u8; 8];
    let desc = one_plane(4, 2, buf.as_mut_ptr(), 4);

    assert_eq!(desc.struct_size, size_of::<ImageDesc>());
    assert!(desc.is_abi_current());
    assert!(desc.has_cicp());
}

#[test]
fn image_desc_reads_its_colour_group_only_from_a_prefix_that_covers_it() {
    let desc = bare_desc();

    let older = sized(desc, ImageDesc::MIN_SIZE);
    assert!(older.is_compatible());
    assert!(!older.has_cicp());
    assert!(!sized(desc, ImageDesc::MIN_SIZE - 1).is_compatible());
    assert!(sized(desc, ImageDesc::CICP_END).has_cicp());
    assert!(!sized(desc, ImageDesc::CICP_END - 1).has_cicp());
    let newer = sized(desc, size_of::<ImageDesc>() + 8);
    assert!(newer.is_compatible());
    assert!(newer.has_cicp());
}

#[test]
fn is_abi_current_is_an_equality_with_this_builds_layout() {
    // Informational, unlike `is_compatible`: an older and a newer producer are both readable, and
    // neither is current.
    let desc = bare_desc();
    assert!(!sized(desc, size_of::<ImageDesc>() - 8).is_abi_current());
    assert!(!sized(desc, size_of::<ImageDesc>() + 8).is_abi_current());

    let mut cfg = StreamConfig::new(TEST_CODEC, 1, 1, 8);
    cfg.struct_size -= 8;
    assert!(!cfg.is_abi_current());
    let mut enc = EncodeConfig::new(TEST_CODEC, 1);
    enc.struct_size += 8;
    assert!(!enc.is_abi_current());
}

/// What a [`Recorder`] saw: the `out` of its last `decode`, or the image of its last `encode`.
type Seen = Rc<Cell<Option<ImageDesc>>>;

/// A decoder and encoder that accept [`TEST_CODEC`], record the descriptor the shim handed them,
/// and — decoding — report [`REPORTED_CICP`] into `out` whether or not its producer wrote the
/// colour group, so a write-back the shim should have withheld is visible. It also claims the
/// widest `struct_size` there is, so a shim that judged the write-back from the copy after the
/// call, rather than from the producer's size, would write past a shorter producer's `out`.
struct Recorder(Seen);

impl Decoder for Recorder {
    fn supports(&mut self, cfg: &StreamConfig) -> bool {
        cfg.codec_id == TEST_CODEC
    }

    fn decode(&mut self, _cfg: &StreamConfig, _codestream: &[u8], out: &mut ImageDesc) -> Status {
        self.0.set(Some(*out));
        let (primaries, transfer, matrix, full) = REPORTED_CICP;
        *out = out.with_cicp(primaries, transfer, matrix, full);
        out.struct_size = usize::MAX;
        Status::OK
    }
}

impl Encoder for Recorder {
    fn supports(&mut self, cfg: &EncodeConfig) -> bool {
        cfg.codec_id == TEST_CODEC
    }

    fn encode(
        &mut self,
        _cfg: &EncodeConfig,
        image: &ImageDesc,
        _sink: &mut dyn FnMut(&[u8]) -> Status,
    ) -> Status {
        self.0.set(Some(*image));
        Status::OK
    }
}

/// A colour group no test backend writes and no default equals, so a value copied from or written
/// to the wrong place shows.
const SENTINEL_CICP: (u16, u16, u16, bool) = (1, 13, 1, true);

/// Calls a lowered decoder's C `decode` slot directly with raw descriptors, as a foreign host
/// would.
fn shim_decode(
    vtable: &DecoderVTable,
    ctx: *mut c_void,
    cfg: &StreamConfig,
    out: *mut ImageDesc,
) -> Status {
    let decode = vtable.decode.expect("lowered vtables fill every slot");
    // SAFETY: `ctx` pairs with `vtable`; `cfg` and `out` are live for the call and each is
    // allocated for at least the `struct_size` it claims.
    unsafe { decode(ctx, cfg, b"stream".as_ptr(), 6, out) }
}

/// A C sink that accepts and discards every chunk.
unsafe extern "C" fn discard(_ctx: *mut c_void, _data: *const u8, _len: usize) -> Status {
    Status::OK
}

#[test]
fn a_lowered_decoder_reads_an_older_out_for_its_prefix_and_writes_back_no_colour() {
    let seen = Seen::default();
    let Lowered { vtable, ctx } = lower_decoder(Box::new(Recorder(Rc::clone(&seen))));
    let (cp, tc, mc, fr) = SENTINEL_CICP;
    // The allocation is a whole current `ImageDesc`, but its producer claims only the mandatory
    // prefix: the colour bytes past it are not the producer's, and are neither read nor written.
    let mut out = sized(bare_desc().with_cicp(cp, tc, mc, fr), ImageDesc::MIN_SIZE);
    let cfg = StreamConfig::new(TEST_CODEC, 4, 2, 8);

    assert_eq!(shim_decode(&vtable, ctx, &cfg, &mut out), Status::OK);

    let got = seen.get().expect("the backend ran");
    assert_eq!((got.width, got.height), (4, 2), "the prefix crossed");
    assert_eq!(
        got.struct_size,
        ImageDesc::MIN_SIZE,
        "the producer's size crossed"
    );
    assert!(!got.has_cicp());
    assert_eq!(
        cicp(&got),
        ImageDesc::UNSIGNALLED,
        "the uncovered tail is the defaults, not the bytes past the producer's prefix"
    );
    assert_eq!(
        cicp(&out),
        SENTINEL_CICP,
        "nothing was written past the prefix"
    );
    // SAFETY: `ctx` came from `lower_decoder` and is destroyed exactly once, here.
    unsafe { vtable.destroy.expect("filled")(ctx) };
}

#[test]
fn a_lowered_decoder_writes_the_reported_colour_back_into_a_covering_out() {
    /// A newer producer's `ImageDesc`: this build's fields, then a field it does not know.
    #[repr(C)]
    struct Newer {
        desc: ImageDesc,
        tail: u64,
    }

    let seen = Seen::default();
    let Lowered { vtable, ctx } = lower_decoder(Box::new(Recorder(Rc::clone(&seen))));
    let mut newer = Newer {
        desc: sized(bare_desc(), size_of::<Newer>()),
        tail: 0xA5A5,
    };
    let cfg = StreamConfig::new(TEST_CODEC, 4, 2, 8);

    let status = shim_decode(&vtable, ctx, &cfg, &raw mut newer.desc);

    assert_eq!(status, Status::OK);
    assert!(seen.get().expect("the backend ran").has_cicp());
    assert_eq!(cicp(&newer.desc), REPORTED_CICP);
    assert_eq!(
        newer.desc.struct_size,
        size_of::<Newer>(),
        "the host's fields stay its own"
    );
    assert_eq!(newer.tail, 0xA5A5, "the unknown field is untouched");
    // SAFETY: as above.
    unsafe { vtable.destroy.expect("filled")(ctx) };
}

#[test]
fn a_lowered_decoder_refuses_descriptors_too_short_for_a_mandatory_field() {
    let (_destroyed, decoder) = counted_decoder();
    let Lowered { vtable, ctx } = lower_decoder(decoder);
    let mut buf = [0u8; 2];
    let full = one_plane(4, 2, buf.as_mut_ptr(), 4);
    let cfg = StreamConfig::new(TEST_CODEC, 4, 2, 8);

    let mut short_out = sized(full, ImageDesc::MIN_SIZE - 1);
    assert_eq!(
        shim_decode(&vtable, ctx, &cfg, &mut short_out),
        Status::UNSUPPORTED
    );
    let mut short_cfg = cfg;
    short_cfg.struct_size = StreamConfig::MIN_SIZE - 1;
    let mut out = full;
    assert_eq!(
        shim_decode(&vtable, ctx, &short_cfg, &mut out),
        Status::UNSUPPORTED
    );
    assert_eq!(buf, [0, 0], "the backend never ran");

    let supports = vtable.supports.expect("filled");
    // SAFETY: `ctx` pairs with `vtable`; each config is live and as long as it claims.
    assert_eq!(unsafe { supports(ctx, &short_cfg) }, Status::UNSUPPORTED);
    let mut oldest = cfg;
    oldest.struct_size = StreamConfig::MIN_SIZE;
    // SAFETY: as above.
    assert_eq!(unsafe { supports(ctx, &oldest) }, Status::OK);
    // SAFETY: `ctx` came from `lower_decoder` and is destroyed exactly once, here.
    unsafe { vtable.destroy.expect("filled")(ctx) };
}

#[test]
fn a_lowered_encoder_reads_an_older_image_for_its_prefix_and_refuses_a_short_one() {
    let seen = Seen::default();
    let Lowered { vtable, ctx } = lower_encoder(Box::new(Recorder(Rc::clone(&seen))));
    let encode = vtable.encode.expect("filled");
    let supports = vtable.supports.expect("filled");
    let cfg = EncodeConfig::new(TEST_CODEC, 80);
    let (cp, tc, mc, fr) = SENTINEL_CICP;
    let older = sized(bare_desc().with_cicp(cp, tc, mc, fr), ImageDesc::MIN_SIZE);

    // SAFETY: `ctx` pairs with `vtable`; the descriptors are live and as long as they claim.
    let status = unsafe { encode(ctx, &cfg, &older, discard, ptr::null_mut()) };
    assert_eq!(status, Status::OK);
    let got = seen.get().expect("the backend ran");
    assert!(!got.has_cicp());
    assert_eq!(cicp(&got), ImageDesc::UNSIGNALLED);

    seen.set(None);
    let short = sized(older, ImageDesc::MIN_SIZE - 1);
    // SAFETY: as above.
    let status = unsafe { encode(ctx, &cfg, &short, discard, ptr::null_mut()) };
    assert_eq!(status, Status::UNSUPPORTED);
    let mut short_cfg = cfg;
    short_cfg.struct_size = EncodeConfig::MIN_SIZE - 1;
    // SAFETY: as above.
    let status = unsafe { encode(ctx, &short_cfg, &older, discard, ptr::null_mut()) };
    assert_eq!(status, Status::UNSUPPORTED);
    assert!(seen.get().is_none(), "the backend never ran");

    // SAFETY: as above.
    assert_eq!(unsafe { supports(ctx, &short_cfg) }, Status::UNSUPPORTED);
    let mut oldest = cfg;
    oldest.struct_size = EncodeConfig::MIN_SIZE;
    // SAFETY: as above.
    assert_eq!(unsafe { supports(ctx, &oldest) }, Status::OK);
    // SAFETY: `ctx` came from `lower_encoder` and is destroyed exactly once, here.
    unsafe { vtable.destroy.expect("filled")(ctx) };
}

#[test]
fn foreign_decoder_rejects_a_vtable_whose_abi_version_is_not_current() {
    let bumped = DecoderVTable {
        abi_version: ABI_VERSION + 1,
        supports: None,
        decode: None,
        destroy: None,
    };
    // SAFETY: `bumped` is a valid vtable; the null ctx is never used because construction fails.
    assert!(unsafe { ForeignDecoder::new(&bumped, ptr::null_mut()) }.is_none());

    // Zero is what an uninitialised or zeroed C struct carries, so it must not pass either; and
    // the previous revision, which held descriptors to `struct_size` equality, must be rebuilt.
    for stale in [0, ABI_VERSION - 1] {
        let vtable = DecoderVTable {
            abi_version: stale,
            supports: None,
            decode: None,
            destroy: None,
        };
        // SAFETY: as above.
        assert!(unsafe { ForeignDecoder::new(&vtable, ptr::null_mut()) }.is_none());
    }
}

#[test]
fn foreign_encoder_rejects_a_vtable_whose_abi_version_is_not_current() {
    let bumped = EncoderVTable {
        abi_version: ABI_VERSION + 1,
        supports: None,
        encode: None,
        destroy: None,
    };
    // SAFETY: `bumped` is a valid vtable; the null ctx is never used because construction fails.
    assert!(unsafe { ForeignEncoder::new(&bumped, ptr::null_mut()) }.is_none());

    for stale in [0, ABI_VERSION - 1] {
        let vtable = EncoderVTable {
            abi_version: stale,
            supports: None,
            encode: None,
            destroy: None,
        };
        // SAFETY: as above.
        assert!(unsafe { ForeignEncoder::new(&vtable, ptr::null_mut()) }.is_none());
    }
}

#[test]
fn the_foreign_constructors_reject_a_null_vtable() {
    // A null vtable is explicitly allowed as *input* by the constructors' contract — it is the
    // shape a C caller passes when it has no backend — and must read as "no adapter", not a
    // dereference.
    // SAFETY: passing null is exactly what the constructor's contract permits.
    assert!(unsafe { ForeignDecoder::new(ptr::null(), ptr::null_mut()) }.is_none());
    // SAFETY: as above.
    assert!(unsafe { ForeignEncoder::new(ptr::null(), ptr::null_mut()) }.is_none());
}
