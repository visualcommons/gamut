//! S3 (issue #238): the pluggable [`HevcDecoder`] hook and the decode pipeline.
//!
//! A `Mock` decoder derives a **deterministic position-dependent gradient** frame from the coded
//! payload (the payload encodes `(base, width, height)`, the config the chroma/bit-depth), so every
//! test asserts golden-exact samples/pixels and a placement/rotation/crop error cannot hide behind a
//! solid colour. The transformative-property and grid tests compare against independently-written
//! reference transforms.

use gamut_color::{ColorRange, ycbcr_to_rgb};
use gamut_core::{Error, ErrorKind, Result};
use gamut_heic::{
    ChromaFormat, DecodedFrame, HeifContainer, HevcConfig, HevcDecoder, iter_nal_units,
};
use gamut_isobmff::{
    ColourInformation, ImageGrid, ImageOverlay, IsoBmffImage, Item, ItemReference, NclxColr,
    Property, PropertyKind, write,
};

// ---- deterministic mock decoder --------------------------------------------------------------

/// The luma value the mock produces at `(x, y)` for a frame keyed by `base`, masked to `bit_depth`.
fn ey(base: u8, x: u32, y: u32, bd: u8) -> u16 {
    ((u32::from(base) + x * 3 + y * 17) as u16) & mask_for(bd)
}
/// The Cb value the mock produces at chroma `(x, y)`.
fn ecb(base: u8, x: u32, y: u32, bd: u8) -> u16 {
    ((u32::from(base) + 40 + x * 5 + y * 11) as u16) & mask_for(bd)
}
/// The Cr value the mock produces at chroma `(x, y)`.
fn ecr(base: u8, x: u32, y: u32, bd: u8) -> u16 {
    ((u32::from(base) + 90 + x * 7 + y * 13) as u16) & mask_for(bd)
}
fn mask_for(bd: u8) -> u16 {
    if bd >= 16 { 0xFFFF } else { (1u16 << bd) - 1 }
}

fn build_frame(base: u8, w: u32, h: u32, chroma: ChromaFormat, bd: u8) -> Result<DecodedFrame> {
    let (wu, hu) = (w as usize, h as usize);
    let y: Vec<u16> = (0..wu * hu)
        .map(|i| ey(base, (i % wu) as u32, (i / wu) as u32, bd))
        .collect();
    let (cb, cr) = if chroma == ChromaFormat::Monochrome {
        (Vec::new(), Vec::new())
    } else {
        let (cw, ch) = chroma.chroma_dimensions(w, h);
        let (cwu, chu) = (cw as usize, ch as usize);
        let cb = (0..cwu * chu)
            .map(|i| ecb(base, (i % cwu) as u32, (i / cwu) as u32, bd))
            .collect();
        let cr = (0..cwu * chu)
            .map(|i| ecr(base, (i % cwu) as u32, (i / cwu) as u32, bd))
            .collect();
        (cb, cr)
    };
    DecodedFrame::new(w, h, bd, chroma, y, cb, cr)
}

/// A recorded `decode_intra` invocation, for asserting the pipeline hands the hook the right inputs.
struct Call {
    nal_length_size: usize,
    payload: Vec<u8>,
    chroma: ChromaFormat,
    bit_depth: u8,
}

#[derive(Default)]
struct Mock {
    calls: Vec<Call>,
}

impl HevcDecoder for Mock {
    fn decode_intra(&mut self, config: &HevcConfig, payload: &[u8]) -> Result<DecodedFrame> {
        let nal = iter_nal_units(payload, config.nal_length_size())
            .next()
            .transpose()?
            .ok_or(Error::InvalidInput("mock: empty payload"))?;
        // Payload NAL = [header0, header1, base, width, height].
        let (base, w, h) = (nal[2], u32::from(nal[3]), u32::from(nal[4]));
        self.calls.push(Call {
            nal_length_size: config.nal_length_size(),
            payload: payload.to_vec(),
            chroma: config.chroma_format(),
            bit_depth: config.bit_depth_luma(),
        });
        build_frame(base, w, h, config.chroma_format(), config.bit_depth_luma())
    }
}

/// A mock that panics if invoked — proves the pipeline rejects a payload before the codec hook.
struct NeverCalled;
impl HevcDecoder for NeverCalled {
    fn decode_intra(&mut self, _c: &HevcConfig, _p: &[u8]) -> Result<DecodedFrame> {
        panic!("decoder must not be invoked");
    }
}

// ---- fixture builders ------------------------------------------------------------------------

fn hvcc_record(chroma_idc: u8, bit_depth: u8) -> Vec<u8> {
    let bd = bit_depth - 8;
    let mut v = vec![0u8; 23];
    v[0] = 1; // configurationVersion
    v[1] = 0x03; // profile_space 0 | tier 0 | profile_idc 3
    v[16] = 0xFC | (chroma_idc & 0x03);
    v[17] = 0xF8 | (bd & 0x07);
    v[18] = 0xF8 | (bd & 0x07);
    v[21] = 0b0000_0011; // ... | lengthSizeMinusOne = 3 (4-byte prefixes)
    v[22] = 0; // numOfArrays
    v
}

/// A 4-byte-length-prefixed IDR (IRAP) NAL carrying `(base, w, h)` for the mock.
fn coded_payload(base: u8, w: u32, h: u32) -> Vec<u8> {
    let nal = [0x26u8, 0x01, base, w as u8, h as u8]; // 0x26 = (19 << 1): IDR_W_RADL
    let mut p = (nal.len() as u32).to_be_bytes().to_vec();
    p.extend_from_slice(&nal);
    p
}

fn prop_hvcc(data: Vec<u8>) -> Property {
    Property {
        essential: true,
        kind: PropertyKind::CodecConfiguration {
            kind: *b"hvcC",
            data,
        },
    }
}

fn colr(matrix: u16, full: bool) -> Property {
    Property {
        essential: false,
        kind: PropertyKind::Colour(ColourInformation::Nclx(NclxColr {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: matrix,
            full_range: full,
        })),
    }
}

fn icc(profile: &[u8]) -> Property {
    Property {
        essential: false,
        kind: PropertyKind::Colour(ColourInformation::UnrestrictedIcc(profile.to_vec())),
    }
}

fn irot(turns: u8) -> Property {
    Property {
        essential: true,
        kind: PropertyKind::Rotation(turns),
    }
}
fn imir(axis: u8) -> Property {
    Property {
        essential: true,
        kind: PropertyKind::Mirror(axis),
    }
}
#[allow(clippy::too_many_arguments)]
fn clap(wn: u32, wd: u32, hn: u32, hd: u32, hon: u32, hod: u32, von: u32, vod: u32) -> Property {
    Property {
        essential: true,
        kind: PropertyKind::CleanAperture {
            width_n: wn,
            width_d: wd,
            height_n: hn,
            height_d: hd,
            horiz_off_n: hon,
            horiz_off_d: hod,
            vert_off_n: von,
            vert_off_d: vod,
        },
    }
}

fn dimg(to: &[u32]) -> ItemReference {
    ItemReference {
        reference_type: *b"dimg",
        to_item_ids: to.to_vec(),
    }
}

fn base_item(id: u32, item_type: [u8; 4]) -> Item {
    Item {
        id,
        item_type,
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: false,
        references: vec![],
        properties: vec![],
        payload: vec![],
    }
}

/// A coded `hvc1` item with the given chroma/bit-depth config and `(base, w, h)` payload.
fn coded_item(
    id: u32,
    chroma_idc: u8,
    bd: u8,
    base: u8,
    w: u32,
    h: u32,
    extra: Vec<Property>,
) -> Item {
    let mut properties = vec![prop_hvcc(hvcc_record(chroma_idc, bd))];
    properties.extend(extra);
    Item {
        properties,
        payload: coded_payload(base, w, h),
        ..base_item(id, *b"hvc1")
    }
}

fn file(primary_id: u32, items: Vec<Item>) -> Vec<u8> {
    write(&IsoBmffImage::new(
        *b"heic",
        vec![*b"heic", *b"mif1"],
        primary_id,
        items,
    ))
    .expect("write model")
}

// ---- coded-item planar decode ----------------------------------------------------------------

#[test]
fn coded_planar_passes_exact_config_and_payload_and_gradient() {
    let payload = coded_payload(20, 4, 4);
    let item = coded_item(1, 1, 8, 20, 4, 4, vec![]); // 4:2:0, 8-bit
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();

    let mut mock = Mock::default();
    let frame = container.image().decode_item_planar(1, &mut mock).unwrap();

    // The hook saw the item's exact payload and the config-derived NAL length / chroma / bit depth.
    assert_eq!(mock.calls.len(), 1);
    assert_eq!(mock.calls[0].nal_length_size, 4);
    assert_eq!(mock.calls[0].payload, payload);
    assert_eq!(mock.calls[0].chroma, ChromaFormat::Yuv420);
    assert_eq!(mock.calls[0].bit_depth, 8);

    // The returned frame is the deterministic gradient, exact.
    assert_eq!((frame.width(), frame.height()), (4, 4));
    assert_eq!(frame.chroma(), ChromaFormat::Yuv420);
    assert_eq!(frame.chroma_dimensions(), (2, 2));
    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(frame.y()[(y * 4 + x) as usize], ey(20, x, y, 8));
        }
    }
    for cy in 0..2 {
        for cx in 0..2 {
            assert_eq!(frame.cb()[(cy * 2 + cx) as usize], ecb(20, cx, cy, 8));
            assert_eq!(frame.cr()[(cy * 2 + cx) as usize], ecr(20, cx, cy, 8));
        }
    }
}

#[test]
fn non_irap_payload_never_reaches_the_decoder() {
    // A VCL slice that is not IRAP (nal_unit_type 1, header byte 0x02) must be refused by the
    // pipeline (validate_still_payload) before the codec hook is asked to decode it.
    let nal = [0x02u8, 0x01, 0xDD];
    let mut payload = (nal.len() as u32).to_be_bytes().to_vec();
    payload.extend_from_slice(&nal);
    let item = Item {
        properties: vec![prop_hvcc(hvcc_record(1, 8))],
        payload,
        ..base_item(1, *b"hvc1")
    };
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    // `NeverCalled` panics if invoked; the error proves it was not.
    assert!(matches!(
        container.image().decode_item_planar(1, &mut NeverCalled),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

#[test]
fn essential_unknown_property_is_refused() {
    let bad = Property {
        essential: true,
        kind: PropertyKind::Other {
            kind: *b"zzzz",
            data: vec![1, 2, 3],
        },
    };
    let item = coded_item(1, 1, 8, 0, 4, 4, vec![bad]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        container
            .image()
            .decode_item_planar(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
    // The RGBA surface refuses it too.
    assert!(matches!(
        container.image().decode_item_rgba8(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
}

#[test]
fn non_hevc_coded_item_is_unsupported() {
    let item = Item {
        properties: vec![Property {
            essential: true,
            kind: PropertyKind::CodecConfiguration {
                kind: *b"av1C",
                data: vec![0x81, 0, 0, 0],
            },
        }],
        payload: vec![0xAA],
        ..base_item(1, *b"av01")
    };
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        container
            .image()
            .decode_item_planar(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
}

// ---- DecodedFrame::new validation ------------------------------------------------------------

#[test]
fn decoded_frame_new_validates_plane_lengths_and_bit_depth() {
    // 4:4:4 — chroma == luma.
    assert!(
        DecodedFrame::new(
            2,
            2,
            8,
            ChromaFormat::Yuv444,
            vec![0; 4],
            vec![0; 4],
            vec![0; 4]
        )
        .is_ok()
    );
    // 4:2:2 — chroma ceil(w/2) x h = 1 x 2.
    assert!(
        DecodedFrame::new(
            2,
            2,
            8,
            ChromaFormat::Yuv422,
            vec![0; 4],
            vec![0; 2],
            vec![0; 2]
        )
        .is_ok()
    );
    // 4:2:0 even — 2 x 2 chroma.
    assert!(
        DecodedFrame::new(
            4,
            4,
            8,
            ChromaFormat::Yuv420,
            vec![0; 16],
            vec![0; 4],
            vec![0; 4]
        )
        .is_ok()
    );
    // 4:2:0 ODD dims — ceiling division: 5x3 luma ⇒ 3x2 chroma = 6.
    assert!(
        DecodedFrame::new(
            5,
            3,
            8,
            ChromaFormat::Yuv420,
            vec![0; 15],
            vec![0; 6],
            vec![0; 6]
        )
        .is_ok()
    );
    assert!(
        DecodedFrame::new(
            5,
            3,
            8,
            ChromaFormat::Yuv420,
            vec![0; 15],
            vec![0; 4],
            vec![0; 4]
        )
        .is_err()
    );
    // Monochrome — chroma must be empty.
    assert!(
        DecodedFrame::new(
            2,
            2,
            8,
            ChromaFormat::Monochrome,
            vec![0; 4],
            vec![],
            vec![]
        )
        .is_ok()
    );
    assert!(
        DecodedFrame::new(
            2,
            2,
            8,
            ChromaFormat::Monochrome,
            vec![0; 4],
            vec![0],
            vec![]
        )
        .is_err()
    );
    // Wrong luma length.
    assert!(
        DecodedFrame::new(
            2,
            2,
            8,
            ChromaFormat::Yuv444,
            vec![0; 3],
            vec![0; 4],
            vec![0; 4]
        )
        .is_err()
    );
    // Bit-depth bounds: 8 and 16 ok; 7 and 17 rejected.
    assert!(
        DecodedFrame::new(
            1,
            1,
            16,
            ChromaFormat::Monochrome,
            vec![0; 1],
            vec![],
            vec![]
        )
        .is_ok()
    );
    assert!(
        DecodedFrame::new(
            1,
            1,
            7,
            ChromaFormat::Monochrome,
            vec![0; 1],
            vec![],
            vec![]
        )
        .is_err()
    );
    assert!(
        DecodedFrame::new(
            1,
            1,
            17,
            ChromaFormat::Monochrome,
            vec![0; 1],
            vec![],
            vec![]
        )
        .is_err()
    );
    // Zero dimensions rejected.
    assert!(DecodedFrame::new(0, 2, 8, ChromaFormat::Monochrome, vec![], vec![], vec![]).is_err());
}

// ---- grid ------------------------------------------------------------------------------------

fn grid_item(id: u32, rows: u16, columns: u16, ow: u32, oh: u32, tiles: &[u32]) -> Item {
    let payload = ImageGrid {
        rows,
        columns,
        output_width: ow,
        output_height: oh,
    }
    .to_bytes()
    .unwrap();
    Item {
        references: vec![dimg(tiles)],
        payload,
        ..base_item(id, *b"grid")
    }
}

/// A hidden monochrome tile of the given base at 2x2.
fn mono_tile(id: u32, base: u8) -> Item {
    Item {
        hidden: true,
        ..coded_item(id, 0, 8, base, 2, 2, vec![])
    }
}

#[test]
fn grid_assembles_row_major_tiles_and_crops() {
    // 2x2 grid of 2x2 monochrome tiles ⇒ 4x4 canvas, cropped to 3x3 output. Distinct bases give
    // each tile a distinct gradient so a swapped/misplaced tile is caught.
    let tiles = [
        mono_tile(2, 10),
        mono_tile(3, 40),
        mono_tile(4, 70),
        mono_tile(5, 100),
    ];
    let bases = [10u8, 40, 70, 100];
    let grid = grid_item(1, 2, 2, 3, 3, &[2, 3, 4, 5]);
    let bytes = file(
        1,
        vec![
            grid,
            tiles[0].clone(),
            tiles[1].clone(),
            tiles[2].clone(),
            tiles[3].clone(),
        ],
    );
    let container = HeifContainer::parse(&bytes).unwrap();

    let frame = container
        .image()
        .decode_item_planar(1, &mut Mock::default())
        .unwrap();
    assert_eq!((frame.width(), frame.height()), (3, 3));
    assert_eq!(frame.chroma(), ChromaFormat::Monochrome);
    // Independent reference: each output pixel comes from its covering tile's own gradient.
    for oy in 0..3u32 {
        for ox in 0..3u32 {
            let (trow, iy) = (oy / 2, oy % 2);
            let (tcol, ix) = (ox / 2, ox % 2);
            let base = bases[(trow * 2 + tcol) as usize];
            assert_eq!(
                frame.y()[(oy * 3 + ox) as usize],
                ey(base, ix, iy, 8),
                "px ({ox},{oy})"
            );
        }
    }
}

#[test]
fn grid_planar_and_rgba_agree() {
    let tiles = [
        mono_tile(2, 10),
        mono_tile(3, 40),
        mono_tile(4, 70),
        mono_tile(5, 100),
    ];
    let grid = grid_item(1, 2, 2, 3, 3, &[2, 3, 4, 5]);
    let bytes = file(
        1,
        vec![
            grid,
            tiles[0].clone(),
            tiles[1].clone(),
            tiles[2].clone(),
            tiles[3].clone(),
        ],
    );
    let container = HeifContainer::parse(&bytes).unwrap();

    let planar = container
        .image()
        .decode_item_planar(1, &mut Mock::default())
        .unwrap();
    let rgba = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    assert_eq!((rgba.width(), rgba.height()), (3, 3));
    // Grid has no colr ⇒ default BT.601 limited ⇒ monochrome luma is range-expanded to gray.
    for i in 0..9usize {
        let g = expand_limited(planar.y()[i]);
        let px = &rgba.as_samples()[i * 4..i * 4 + 4];
        assert_eq!(px, &[g, g, g, 255]);
    }
    // Pin the expansion with a literal anchor: pixel (0,0) has luma ey(10,0,0)=10 ⇒ (10-16)*255/219
    // clamps to 0.
    assert_eq!(planar.y()[0], 10);
    assert_eq!(rgba.as_samples()[0], 0);
}

fn expand_limited(y: u16) -> u8 {
    (((i32::from(y) - 16) * 255 + 109) / 219).clamp(0, 255) as u8
}

#[test]
fn grid_non_uniform_tiles_are_unsupported() {
    // Mixed tile size.
    let big = Item {
        hidden: true,
        ..coded_item(3, 0, 8, 40, 2, 3, vec![])
    };
    let g = grid_item(1, 1, 2, 4, 2, &[2, 3]);
    let bytes = file(1, vec![g, mono_tile(2, 10), big]);
    let c = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        c.image().decode_item_planar(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));

    // Mixed chroma.
    let chroma_tile = Item {
        hidden: true,
        ..coded_item(3, 1, 8, 40, 2, 2, vec![])
    };
    let g = grid_item(1, 1, 2, 4, 2, &[2, 3]);
    let bytes = file(1, vec![g, mono_tile(2, 10), chroma_tile]);
    let c = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        c.image().decode_item_planar(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));

    // Mixed bit depth.
    let deep = Item {
        hidden: true,
        ..coded_item(3, 0, 10, 40, 2, 2, vec![])
    };
    let g = grid_item(1, 1, 2, 4, 2, &[2, 3]);
    let bytes = file(1, vec![g, mono_tile(2, 10), deep]);
    let c = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        c.image().decode_item_planar(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
}

// ---- iden ------------------------------------------------------------------------------------

#[test]
fn iden_passes_through_and_applies_its_own_transforms() {
    // iden (id 1, irot=1) → coded source (id 2). Planar is a pure passthrough of the source; RGBA
    // applies iden's rotation.
    let source = Item {
        hidden: true,
        ..coded_item(2, 3, 8, 30, 4, 2, vec![colr(0, true)])
    };
    let iden = Item {
        references: vec![dimg(&[2])],
        properties: vec![irot(1), colr(0, true)],
        ..base_item(1, *b"iden")
    };
    let bytes = file(1, vec![iden, source]);
    let container = HeifContainer::parse(&bytes).unwrap();

    // Planar passthrough: iden frame == source frame.
    let via_iden = container
        .image()
        .decode_item_planar(1, &mut Mock::default())
        .unwrap();
    let via_source = container
        .image()
        .decode_item_planar(2, &mut Mock::default())
        .unwrap();
    assert_eq!(via_iden, via_source);

    // RGBA: identity base rotated once CCW.
    let base = identity_base_rgba(30, 4, 2);
    let (want, ww, wh) = ref_rotate_ccw(&base, 4, 2);
    let got = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    assert_eq!((got.width(), got.height()), (ww, wh));
    assert_eq!(got.as_samples(), &want[..]);
}

// ---- transformative properties (on RGBA) -----------------------------------------------------

/// The RGBA the identity (mc=0, 4:4:4) colour path yields for the mock's gradient: R=Cr, G=Y, B=Cb.
fn identity_base_rgba(base: u8, w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let o = ((y * w + x) * 4) as usize;
            out[o] = ecr(base, x, y, 8) as u8;
            out[o + 1] = ey(base, x, y, 8) as u8;
            out[o + 2] = ecb(base, x, y, 8) as u8;
            out[o + 3] = 255;
        }
    }
    out
}

/// Independent 90° CCW rotation by forward scatter (input `(x,y)` → output `(y, w-1-x)`).
fn ref_rotate_ccw<T: Copy + Default>(src: &[T], w: u32, h: u32) -> (Vec<T>, u32, u32) {
    let (wu, hu) = (w as usize, h as usize);
    let (nw, nh) = (hu, wu);
    let mut out = vec![T::default(); nw * nh * 4];
    for y in 0..hu {
        for x in 0..wu {
            let (ox, oy) = (y, wu - 1 - x);
            out[(oy * nw + ox) * 4..(oy * nw + ox) * 4 + 4]
                .copy_from_slice(&src[(y * wu + x) * 4..(y * wu + x) * 4 + 4]);
        }
    }
    (out, h, w)
}

/// Independent mirror by forward scatter.
fn ref_mirror<T: Copy + Default>(src: &[T], w: u32, h: u32, axis: u8) -> Vec<T> {
    let (wu, hu) = (w as usize, h as usize);
    let mut out = vec![T::default(); wu * hu * 4];
    for y in 0..hu {
        for x in 0..wu {
            let (dx, dy) = if axis == 1 {
                (wu - 1 - x, y)
            } else {
                (x, hu - 1 - y)
            };
            out[(dy * wu + dx) * 4..(dy * wu + dx) * 4 + 4]
                .copy_from_slice(&src[(y * wu + x) * 4..(y * wu + x) * 4 + 4]);
        }
    }
    out
}

fn ref_crop<T: Copy + Default>(src: &[T], w: u32, left: u32, top: u32, cw: u32, ch: u32) -> Vec<T> {
    let (wu, cwu) = (w as usize, cw as usize);
    let mut out = vec![T::default(); (cw * ch * 4) as usize];
    for y in 0..ch as usize {
        for x in 0..cwu {
            let si = ((top as usize + y) * wu + (left as usize + x)) * 4;
            out[(y * cwu + x) * 4..(y * cwu + x) * 4 + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    out
}

fn decode_rgba_with_props(base: u8, w: u32, h: u32, props: Vec<Property>) -> (Vec<u8>, u32, u32) {
    let mut properties = vec![colr(0, true)];
    properties.extend(props);
    let item = coded_item(1, 3, 8, base, w, h, properties); // identity 4:4:4
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let buf = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    (buf.as_samples().to_vec(), buf.width(), buf.height())
}

#[test]
fn irot_all_four_rotations_are_golden() {
    let base = identity_base_rgba(50, 4, 2);
    for turns in 0..=3u8 {
        let mut want = base.clone();
        let (mut ww, mut wh) = (4u32, 2u32);
        for _ in 0..turns {
            let r = ref_rotate_ccw(&want, ww, wh);
            want = r.0;
            ww = r.1;
            wh = r.2;
        }
        let (got, gw, gh) = decode_rgba_with_props(50, 4, 2, vec![irot(turns)]);
        assert_eq!((gw, gh), (ww, wh), "turns {turns}");
        assert_eq!(got, want, "turns {turns}");
    }
}

#[test]
fn imir_both_axes_are_golden() {
    let base = identity_base_rgba(60, 4, 2);
    // Pin the two spec-defined permutations directly. The asymmetric 4x2 raster numbers its
    // pixels in row-major order, so neither expected order repeats the implementation's branch.
    for (axis, order) in [
        (0u8, [4usize, 5, 6, 7, 0, 1, 2, 3]),
        (1u8, [3usize, 2, 1, 0, 7, 6, 5, 4]),
    ] {
        let want: Vec<u8> = order
            .into_iter()
            .flat_map(|i| base[i * 4..i * 4 + 4].iter().copied())
            .collect();
        let (got, gw, gh) = decode_rgba_with_props(60, 4, 2, vec![imir(axis)]);
        assert_eq!((gw, gh), (4, 2), "axis {axis}");
        assert_eq!(got, want, "axis {axis}");
    }
}

#[test]
fn clap_signed_offset_crop_is_golden() {
    // 4x2 image, crop 2x2. left = (4-2)/2 + horizOff. With horizOff_n = -1 ⇒ left = 0; top = 0.
    let base = identity_base_rgba(70, 4, 2);
    let want = ref_crop(&base, 4, 0, 0, 2, 2);
    let neg_one = (-1i32) as u32;
    let (got, gw, gh) = decode_rgba_with_props(70, 4, 2, vec![clap(2, 1, 2, 1, neg_one, 1, 0, 1)]);
    assert_eq!((gw, gh), (2, 2));
    assert_eq!(got, want);

    // horizOff_n = +1 ⇒ left = 2, crop the right half.
    let want = ref_crop(&base, 4, 2, 0, 2, 2);
    let (got, _, _) = decode_rgba_with_props(70, 4, 2, vec![clap(2, 1, 2, 1, 1, 1, 0, 1)]);
    assert_eq!(got, want);
}

#[test]
fn clap_non_integer_and_out_of_bounds_are_invalid() {
    // Non-integer crop width (3/2).
    let item = coded_item(
        1,
        3,
        8,
        0,
        4,
        2,
        vec![colr(0, true), clap(3, 2, 2, 1, 0, 1, 0, 1)],
    );
    let bytes = file(1, vec![item]);
    let c = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        c.image().decode_item_rgba8(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));

    // Non-integer offset (0.5 via horizOff 1/2 on an even (w-crop)).
    let item = coded_item(
        1,
        3,
        8,
        0,
        4,
        2,
        vec![colr(0, true), clap(2, 1, 2, 1, 1, 2, 0, 1)],
    );
    let bytes = file(1, vec![item]);
    let c = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        c.image().decode_item_rgba8(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));

    // Out of bounds: crop 4x2 shifted right by 1 ⇒ right edge at 5 > 4.
    let item = coded_item(
        1,
        3,
        8,
        0,
        4,
        2,
        vec![colr(0, true), clap(4, 1, 2, 1, 1, 1, 0, 1)],
    );
    let bytes = file(1, vec![item]);
    let c = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        c.image().decode_item_rgba8(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

#[test]
fn combined_transforms_apply_in_ipma_order() {
    // MIAF order [clap, irot, imir].
    let base = identity_base_rgba(80, 4, 2);
    let cropped = ref_crop(&base, 4, 1, 0, 2, 2);
    let (rot, rw, rh) = ref_rotate_ccw(&cropped, 2, 2);
    let want = ref_mirror(&rot, rw, rh, 0);
    let (got, gw, gh) = decode_rgba_with_props(
        80,
        4,
        2,
        vec![clap(2, 1, 2, 1, 0, 1, 0, 1), irot(1), imir(0)],
    );
    assert_eq!((gw, gh), (rw, rh));
    assert_eq!(got, want);

    // Non-MIAF order [irot, clap]: applied exactly as listed. irot(1) makes 4x2 → 2x4, then clap
    // crops a 2x2 window at top = (4-2)/2 = 1.
    let (rot, rw, _) = ref_rotate_ccw(&base, 4, 2);
    let want = ref_crop(&rot, rw, 0, 1, 2, 2);
    let (got, gw, gh) =
        decode_rgba_with_props(80, 4, 2, vec![irot(1), clap(2, 1, 2, 1, 0, 1, 0, 1)]);
    assert_eq!((gw, gh), (2, 2));
    assert_eq!(got, want);
}

// ---- colour conversion -----------------------------------------------------------------------

#[test]
fn bt601_limited_and_full_range_are_golden() {
    // 4x4, 4:2:0, matrix 6 (BT.601). Limited vs full must differ; assert both against gamut_color.
    for full in [false, true] {
        let range = if full {
            ColorRange::Full
        } else {
            ColorRange::Limited
        };
        let item = coded_item(1, 1, 8, 15, 4, 4, vec![colr(6, full)]);
        let bytes = file(1, vec![item]);
        let container = HeifContainer::parse(&bytes).unwrap();
        let rgba = container
            .image()
            .decode_item_rgba8(1, &mut Mock::default())
            .unwrap();
        for y in 0..4u32 {
            for x in 0..4u32 {
                let (cx, cy) = (x / 2, y / 2);
                let yv = ey(15, x, y, 8) as u8;
                let cb = ecb(15, cx, cy, 8) as u8;
                let cr = ecr(15, cx, cy, 8) as u8;
                let (r, g, b) = ycbcr_to_rgb(yv, cb, cr, range);
                let o = ((y * 4 + x) * 4) as usize;
                assert_eq!(
                    &rgba.as_samples()[o..o + 4],
                    &[r, g, b, 255],
                    "full={full} ({x},{y})"
                );
            }
        }
    }
    // The two ranges genuinely differ somewhere (guards against a range-ignoring conversion).
    let li = {
        let item = coded_item(1, 1, 8, 15, 4, 4, vec![colr(6, false)]);
        let b = file(1, vec![item]);
        let c = HeifContainer::parse(&b).unwrap();
        c.image()
            .decode_item_rgba8(1, &mut Mock::default())
            .unwrap()
            .into_samples()
    };
    let fu = {
        let item = coded_item(1, 1, 8, 15, 4, 4, vec![colr(6, true)]);
        let b = file(1, vec![item]);
        let c = HeifContainer::parse(&b).unwrap();
        c.image()
            .decode_item_rgba8(1, &mut Mock::default())
            .unwrap()
            .into_samples()
    };
    assert_ne!(li, fu);
}

#[test]
fn identity_matrix_maps_gbr_directly() {
    let item = coded_item(1, 3, 8, 33, 3, 2, vec![colr(0, true)]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let rgba = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    assert_eq!(rgba.as_samples(), &identity_base_rgba(33, 3, 2)[..]);
}

#[test]
fn rgba_presentation_prefers_nclx_over_icc_in_either_order() {
    let nclx_only = {
        let item = coded_item(1, 3, 8, 33, 3, 2, vec![colr(0, true)]);
        let bytes = file(1, vec![item]);
        HeifContainer::parse(&bytes)
            .unwrap()
            .image()
            .decode_item_rgba8(1, &mut Mock::default())
            .unwrap()
            .into_samples()
    };

    for properties in [
        vec![icc(b"first"), colr(0, true)],
        vec![colr(0, true), icc(b"second")],
    ] {
        let item = coded_item(1, 3, 8, 33, 3, 2, properties);
        let bytes = file(1, vec![item]);
        let container = HeifContainer::parse(&bytes).unwrap();
        let got = container
            .image()
            .decode_item_rgba8(1, &mut Mock::default())
            .unwrap();
        assert_eq!(got.as_samples(), nclx_only.as_slice());
    }
}

#[test]
fn first_colour_accessor_and_icc_only_fallback_are_unchanged() {
    let dual = coded_item(1, 3, 8, 33, 3, 2, vec![icc(b"first"), colr(0, true)]);
    let bytes = file(1, vec![dual]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        container.image().item(1).unwrap().colour(),
        Some(ColourInformation::UnrestrictedIcc(profile)) if profile == b"first"
    ));

    let icc_only = coded_item(1, 3, 8, 33, 3, 2, vec![icc(b"only")]);
    let bytes = file(1, vec![icc_only]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(
        container
            .image()
            .decode_item_planar(1, &mut Mock::default())
            .is_ok()
    );
    assert!(matches!(
        container.image().decode_item_rgba8(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
}

#[test]
fn monochrome_limited_expansion_is_golden() {
    // No colr ⇒ default BT.601 limited ⇒ monochrome luma range-expanded.
    let item = coded_item(1, 0, 8, 125, 3, 2, vec![]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let rgba = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    for y in 0..2u32 {
        for x in 0..3u32 {
            let g = expand_limited(ey(125, x, y, 8));
            let o = ((y * 3 + x) * 4) as usize;
            assert_eq!(&rgba.as_samples()[o..o + 4], &[g, g, g, 255]);
        }
    }
    // Literal anchor: pixel (0,0) luma = 125 ⇒ (125-16)*255/219 = 127.
    assert_eq!(rgba.as_samples()[0], 127);
}

#[test]
fn unsupported_colour_falls_back_to_planar_only() {
    // YCgCo (matrix 8) is a different transform family: planar decodes; both RGBA surfaces refuse.
    let item = coded_item(1, 1, 8, 10, 4, 4, vec![colr(8, false)]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(
        container
            .image()
            .decode_item_planar(1, &mut Mock::default())
            .is_ok()
    );
    assert!(matches!(
        container.image().decode_item_rgba8(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
    assert!(matches!(
        container.image().decode_item_rgba16(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
}

#[test]
fn ten_bit_frame_is_rejected_by_the_eight_bit_surface() {
    let item = coded_item(1, 1, 10, 200, 4, 4, vec![colr(6, false)]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let frame = container
        .image()
        .decode_item_planar(1, &mut Mock::default())
        .unwrap();
    assert_eq!(frame.bit_depth(), 10);
    // Narrowing to 8 bits would be silent quality loss, so the narrow surface still declines...
    assert!(matches!(
        container.image().decode_item_rgba8(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
    // ...while the wide surface presents it.
    let rgba = container
        .image()
        .decode_item_rgba16(1, &mut Mock::default())
        .unwrap();
    assert_eq!((rgba.width(), rgba.height()), (4, 4));
    assert_eq!(rgba.as_samples().len(), 4 * 4 * 4);
}

#[test]
fn unmodeled_bit_depth_is_matrixed_only_by_refusal() {
    // `DecodedFrame` admits 9-bit, but no CICP de-matrixing is modeled for it: an explicit refusal
    // beats silently reinterpreting it as 8- or 10-bit.
    let item = coded_item(1, 1, 9, 200, 4, 4, vec![colr(6, false)]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert_eq!(
        container
            .image()
            .decode_item_planar(1, &mut Mock::default())
            .unwrap()
            .bit_depth(),
        9
    );
    assert!(matches!(
        container.image().decode_item_rgba16(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
    // Monochrome never consults a matrix, so it presents at any depth the frame can carry.
    let mono = coded_item(1, 0, 9, 200, 4, 4, vec![colr(6, false)]);
    let bytes = file(1, vec![mono]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(
        container
            .image()
            .decode_item_rgba16(1, &mut Mock::default())
            .is_ok()
    );
}

// ---- high-bit-depth presentation (issue #303) ------------------------------------------------

/// The `(Kr, Kb)` of an nclx matrix code point, for the reference conversion below.
fn luma_weights(matrix: u16) -> (f64, f64) {
    match matrix {
        1 => (0.2126, 0.0722),
        5 | 6 => (0.299, 0.114),
        9 => (0.2627, 0.0593),
        other => panic!("no reference weights for matrix {other}"),
    }
}

/// An independent `f64` reference for the wide surface, written from the H.273 §8.3 equations:
/// de-matrix at the coded depth, then apply the documented full-16-bit rescale.
fn ref_rgb16(matrix: u16, full: bool, bd: u8, y: u16, cb: u16, cr: u16) -> (u16, u16, u16) {
    let (kr, kb) = luma_weights(matrix);
    let max_in = f64::from((1u32 << bd) - 1);
    let (yn, cbn, crn) = if full {
        let mid = f64::from(1u32 << (bd - 1));
        (
            f64::from(y) / max_in,
            (f64::from(cb) - mid) / max_in,
            (f64::from(cr) - mid) / max_in,
        )
    } else {
        let s = f64::from(1u32 << (bd - 8));
        (
            (f64::from(y) - 16.0 * s) / (219.0 * s),
            (f64::from(cb) - 128.0 * s) / (224.0 * s),
            (f64::from(cr) - 128.0 * s) / (224.0 * s),
        )
    };
    let kg = 1.0 - kr - kb;
    let r = yn + 2.0 * (1.0 - kr) * crn;
    let b = yn + 2.0 * (1.0 - kb) * cbn;
    let g = yn - (2.0 * kb * (1.0 - kb) / kg) * cbn - (2.0 * kr * (1.0 - kr) / kg) * crn;
    // Quantize at the coded depth (what the surface actually resolves), then rescale as documented.
    let q = |v: f64| {
        let coded = (v.clamp(0.0, 1.0) * max_in).round() as u32;
        let max = max_in as u32;
        ((u64::from(coded) * 65535 + u64::from(max) / 2) / u64::from(max)) as u16
    };
    (q(r), q(g), q(b))
}

/// One coded-depth LSB expressed on the 16-bit surface — the tolerance against `ref_rgb16`, since
/// the surface's precision is inherently that of the coded frame.
fn coded_lsb16(bd: u8) -> u16 {
    (65535 / ((1u32 << bd) - 1)) as u16 + 1
}

/// Decodes a single coded item to the wide surface.
fn decode_rgba16(
    chroma_idc: u8,
    bd: u8,
    base: u8,
    w: u32,
    h: u32,
    props: Vec<Property>,
) -> Vec<u16> {
    let item = coded_item(1, chroma_idc, bd, base, w, h, props);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    container
        .image()
        .decode_item_rgba16(1, &mut Mock::default())
        .unwrap()
        .into_samples()
}

/// Asserts a 4:2:0 wide-surface decode matches the reference conversion pixel by pixel.
fn assert_matches_reference(matrix: u16, full: bool, bd: u8, base: u8, w: u32, h: u32) {
    let got = decode_rgba16(1, bd, base, w, h, vec![colr(matrix, full)]);
    let tol = coded_lsb16(bd);
    for y in 0..h {
        for x in 0..w {
            let (cx, cy) = (x / 2, y / 2);
            let want = ref_rgb16(
                matrix,
                full,
                bd,
                ey(base, x, y, bd),
                ecb(base, cx, cy, bd),
                ecr(base, cx, cy, bd),
            );
            let o = ((y * w + x) * 4) as usize;
            for (c, want) in [want.0, want.1, want.2].into_iter().enumerate() {
                assert!(
                    got[o + c].abs_diff(want) <= tol,
                    "matrix={matrix} full={full} bd={bd} ({x},{y}) ch{c}: got {} want {want}",
                    got[o + c]
                );
            }
            assert_eq!(got[o + 3], 65535, "alpha ({x},{y})");
        }
    }
}

#[test]
fn bt709_ten_bit_limited_matches_the_reference() {
    assert_matches_reference(1, false, 10, 200, 4, 4);
}

#[test]
fn bt2020_ten_bit_limited_and_full_match_the_reference() {
    // The issue's headline case: 10-bit BT.2020, both ranges.
    assert_matches_reference(9, false, 10, 200, 4, 4);
    assert_matches_reference(9, true, 10, 200, 4, 4);
    // The two ranges genuinely differ, so a range-ignoring conversion cannot pass.
    let limited = decode_rgba16(1, 10, 200, 4, 4, vec![colr(9, false)]);
    let full = decode_rgba16(1, 10, 200, 4, 4, vec![colr(9, true)]);
    assert_ne!(limited, full);
}

#[test]
fn twelve_bit_matches_the_reference() {
    // Pins that the white level is the frame's own (4095), not 1023 or 255.
    assert_matches_reference(1, false, 12, 200, 4, 4);
}

#[test]
fn matrices_differ_at_ten_bit() {
    // A tolerance-based comparison passes even if the matrix argument is ignored; this does not.
    let of = |m| decode_rgba16(1, 10, 200, 4, 4, vec![colr(m, false)]);
    let (a, b, c) = (of(1), of(6), of(9));
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
}

#[test]
fn eight_bit_widens_exactly_by_257() {
    // The strongest invariant on the wide surface: 8-bit content is carried losslessly, because
    // 65535 == 255 * 257. Covers every matrix the surface supports, both ranges, plus monochrome
    // and identity, and pins opaque alpha at both widths.
    for (chroma_idc, matrix) in [(3u8, 0u16), (1, 1), (1, 6), (1, 9), (0, 6)] {
        for full in [false, true] {
            let props = vec![colr(matrix, full)];
            let item = coded_item(1, chroma_idc, 8, 33, 4, 4, props.clone());
            let bytes = file(1, vec![item]);
            let container = HeifContainer::parse(&bytes).unwrap();
            let narrow = container
                .image()
                .decode_item_rgba8(1, &mut Mock::default())
                .unwrap()
                .into_samples();
            let wide = decode_rgba16(chroma_idc, 8, 33, 4, 4, props);
            assert_eq!(narrow.len(), wide.len());
            for (i, (&n, &w)) in narrow.iter().zip(&wide).enumerate() {
                assert_eq!(
                    u16::from(n) * 257,
                    w,
                    "matrix={matrix} full={full} sample {i}"
                );
            }
        }
    }
}

#[test]
fn bt601_at_eight_bit_still_uses_the_libwebp_inverse() {
    // The deliberate carve-out: 8-bit BT.601 keeps gamut-color's libwebp-exact integer inverse, so
    // the output this crate has always produced stays byte-identical. Everything else routes
    // through the H.273-derived converter.
    for full in [false, true] {
        let range = if full {
            ColorRange::Full
        } else {
            ColorRange::Limited
        };
        let item = coded_item(1, 1, 8, 15, 4, 4, vec![colr(6, full)]);
        let bytes = file(1, vec![item]);
        let container = HeifContainer::parse(&bytes).unwrap();
        let rgba = container
            .image()
            .decode_item_rgba8(1, &mut Mock::default())
            .unwrap();
        for y in 0..4u32 {
            for x in 0..4u32 {
                let (cx, cy) = (x / 2, y / 2);
                let (r, g, b) = ycbcr_to_rgb(
                    ey(15, x, y, 8) as u8,
                    ecb(15, cx, cy, 8) as u8,
                    ecr(15, cx, cy, 8) as u8,
                    range,
                );
                let o = ((y * 4 + x) * 4) as usize;
                assert_eq!(&rgba.as_samples()[o..o + 4], &[r, g, b, 255]);
            }
        }
    }
}

#[test]
fn monochrome_ten_bit_expansion_is_golden() {
    let got = decode_rgba16(0, 10, 200, 3, 2, vec![]);
    let widen = |coded: i64| ((coded as u64 * 65535 + 511) / 1023) as u16;
    for y in 0..2u32 {
        for x in 0..3u32 {
            // Studio swing at 10-bit — black 64, span 876 — expanded at the coded depth, then
            // widened, which is the precision model the whole surface uses.
            let luma = i64::from(ey(200, x, y, 10));
            let coded = (((luma - 64) * 1023 + 438) / 876).clamp(0, 1023);
            let want = widen(coded);
            let o = ((y * 3 + x) * 4) as usize;
            assert_eq!(&got[o..o + 4], &[want, want, want, 65535], "({x},{y})");
        }
    }
    // Literal anchor: pixel (0,0) luma = 200 ⇒ (200-64)*1023/876 = 159 coded ⇒ 10186.
    assert_eq!(got[0], widen(159));
    assert_eq!(got[0], 10186);
    // Full range is a plain rescale of the same planes, and differs.
    let full = decode_rgba16(0, 10, 200, 3, 2, vec![colr(6, true)]);
    assert_ne!(got, full);
    let luma = u64::from(ey(200, 0, 0, 10));
    assert_eq!(full[0], ((luma * 65535 + 511) / 1023) as u16);
}

#[test]
fn identity_matrix_maps_gbr_directly_at_ten_bit() {
    let got = decode_rgba16(3, 10, 200, 3, 2, vec![colr(0, true)]);
    let scale = |s: u16| ((u64::from(s) * 65535 + 511) / 1023) as u16;
    for y in 0..2u32 {
        for x in 0..3u32 {
            let o = ((y * 3 + x) * 4) as usize;
            assert_eq!(
                &got[o..o + 4],
                &[
                    scale(ecr(200, x, y, 10)),
                    scale(ey(200, x, y, 10)),
                    scale(ecb(200, x, y, 10)),
                    65535
                ],
                "({x},{y})"
            );
        }
    }
    // Identity still requires 4:4:4 on the wide surface.
    let item = coded_item(1, 1, 10, 200, 4, 4, vec![colr(0, true)]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        container.image().decode_item_rgba16(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
}

#[test]
fn grid_and_irot_on_a_ten_bit_item() {
    // The combination issue #303 calls out: derived-image assembly and a transform on a >8-bit
    // item, presented through the wide surface.
    let tile = |id: u32, base: u8| Item {
        hidden: true,
        ..coded_item(id, 0, 10, base, 2, 2, vec![])
    };
    let bases = [10u8, 40, 70, 100];
    let grid = Item {
        properties: vec![irot(1)],
        ..grid_item(1, 2, 2, 3, 3, &[2, 3, 4, 5])
    };
    let bytes = file(
        1,
        vec![
            grid,
            tile(2, bases[0]),
            tile(3, bases[1]),
            tile(4, bases[2]),
            tile(5, bases[3]),
        ],
    );
    let container = HeifContainer::parse(&bytes).unwrap();
    let rgba = container
        .image()
        .decode_item_rgba16(1, &mut Mock::default())
        .unwrap();

    // Independent reference: assemble the 3x3 monochrome canvas, expand, then rotate.
    let mut flat = vec![0u16; 3 * 3 * 4];
    for oy in 0..3u32 {
        for ox in 0..3u32 {
            let base = bases[((oy / 2) * 2 + ox / 2) as usize];
            let luma = i64::from(ey(base, ox % 2, oy % 2, 10));
            let coded = (((luma - 64) * 1023 + 438) / 876).clamp(0, 1023) as u64;
            let g = ((coded * 65535 + 511) / 1023) as u16;
            let o = ((oy * 3 + ox) * 4) as usize;
            flat[o..o + 4].copy_from_slice(&[g, g, g, 65535]);
        }
    }
    let (want, ww, wh) = ref_rotate_ccw(&flat, 3, 3);
    assert_eq!((rgba.width(), rgba.height()), (ww, wh));
    assert_eq!(rgba.as_samples(), &want[..]);
}

#[test]
fn imir_and_clap_on_a_ten_bit_item() {
    let props = vec![colr(0, true), imir(1), clap(2, 1, 2, 1, 0, 1, 0, 1)];
    let item = coded_item(1, 3, 10, 200, 4, 4, props);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let rgba = container
        .image()
        .decode_item_rgba16(1, &mut Mock::default())
        .unwrap();

    let scale = |s: u16| ((u64::from(s) * 65535 + 511) / 1023) as u16;
    let mut flat = vec![0u16; 4 * 4 * 4];
    for y in 0..4u32 {
        for x in 0..4u32 {
            let o = ((y * 4 + x) * 4) as usize;
            flat[o..o + 4].copy_from_slice(&[
                scale(ecr(200, x, y, 10)),
                scale(ey(200, x, y, 10)),
                scale(ecb(200, x, y, 10)),
                65535,
            ]);
        }
    }
    let mirrored = ref_mirror(&flat, 4, 4, 1);
    let want = ref_crop(&mirrored, 4, 1, 1, 2, 2);
    assert_eq!((rgba.width(), rgba.height()), (2, 2));
    assert_eq!(rgba.as_samples(), &want[..]);
}

#[test]
fn absent_alpha_is_opaque_at_sixteen_bit() {
    let got = decode_rgba16(1, 10, 200, 4, 4, vec![colr(9, false)]);
    for px in got.as_chunks::<4>().0 {
        assert_eq!(px[3], 65535);
    }
}

#[test]
fn decode_primary_rgba16_and_container_forwarders_agree() {
    let item = coded_item(7, 1, 10, 200, 4, 4, vec![colr(9, false)]);
    let bytes = file(7, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let by_id = container
        .image()
        .decode_item_rgba16(7, &mut Mock::default())
        .unwrap()
        .into_samples();
    let primary = container
        .image()
        .decode_primary_rgba16(&mut Mock::default())
        .unwrap()
        .into_samples();
    let fwd_item = container
        .decode_item_rgba16(7, &mut Mock::default())
        .unwrap()
        .into_samples();
    let fwd_primary = container
        .decode_primary_rgba16(&mut Mock::default())
        .unwrap()
        .into_samples();
    assert_eq!(by_id, primary);
    assert_eq!(by_id, fwd_item);
    assert_eq!(by_id, fwd_primary);
}

#[test]
fn alpha_auxiliary_at_ten_bit_merges_onto_the_wide_surface() {
    // The alpha auxiliary's depth is independent of the master's, so exercise a 10-bit master with
    // a 10-bit alpha and an 8-bit master with a 9-bit alpha.
    for (master_bd, alpha_bd) in [(10u8, 10u8), (8, 9)] {
        let master = coded_item(1, 3, master_bd, 33, 2, 2, vec![colr(0, true)]);
        let alpha = alpha_aux_bd(2, 1, 77, 2, 2, alpha_bd);
        let bytes = file(1, vec![master, alpha]);
        let container = HeifContainer::parse(&bytes).unwrap();
        let got = container
            .image()
            .decode_item_rgba16(1, &mut Mock::default())
            .unwrap()
            .into_samples();
        let max = u64::from((1u32 << alpha_bd) - 1);
        for i in 0..4usize {
            let s = u64::from(ey(77, (i % 2) as u32, (i / 2) as u32, alpha_bd));
            let want = ((s * 65535 + max / 2) / max) as u16;
            assert_eq!(
                got[i * 4 + 3],
                want,
                "master_bd={master_bd} alpha_bd={alpha_bd} px {i}"
            );
        }
    }
}

#[test]
fn overlay_blend_rounding_is_observable_on_a_translucent_canvas() {
    // Every other overlay test composites onto an *opaque* canvas, where `da · (MAX - sa)` is
    // always a multiple of MAX and the source-over rounding addends cancel exactly. This one uses
    // a nearly-transparent canvas fill so both addends — the alpha term and the per-channel term —
    // change the result, pinning the `MAX / 2` rounding the blend depends on.
    let src = Item {
        hidden: true,
        ..coded_item(2, 3, 8, 0, 2, 2, vec![colr(0, true)])
    };
    let src_alpha = alpha_aux(3, 2, 0, 2, 2);
    let ov = Item {
        references: vec![dimg(&[2])],
        payload: ImageOverlay {
            // (2, 1, 0) at alpha 1 after `>> 8`.
            canvas_fill_value: [0x0200, 0x0100, 0x0000, 0x0100],
            output_width: 2,
            output_height: 2,
            offsets: vec![(0, 0)],
        }
        .to_bytes()
        .unwrap(),
        ..base_item(1, *b"iovl")
    };
    let bytes = file(1, vec![ov, src, src_alpha]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let got = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();

    // Independent integer reference of ISO/IEC 23008-12 §6.6.2.4 source-over, round-half-up.
    let (da, fill) = (1u32, [2u32, 1, 0]);
    for y in 0..2u32 {
        for x in 0..2u32 {
            let sa = u32::from(ey(0, x, y, 8) as u8);
            let sc = [
                u32::from(ecr(0, x, y, 8) as u8),
                u32::from(ey(0, x, y, 8) as u8),
                u32::from(ecb(0, x, y, 8) as u8),
            ];
            let inv = 255 - sa;
            let out_a = sa + (da * inv + 127) / 255;
            let mut want = [0u8; 4];
            for c in 0..3 {
                let num = sc[c] * sa + (fill[c] * da * inv + 127) / 255;
                want[c] = ((num + out_a / 2) / out_a).min(255) as u8;
            }
            want[3] = out_a as u8;
            let o = ((y * 2 + x) * 4) as usize;
            assert_eq!(&got.as_samples()[o..o + 4], &want, "({x},{y})");
        }
    }
    // Literal anchors: each of these differs if either rounding addend is altered.
    assert_eq!(&got.as_samples()[0..4], &[2, 1, 0, 1]);
    assert_eq!(&got.as_samples()[4..8], &[73, 3, 34, 4]);
    assert_eq!(&got.as_samples()[8..12], &[97, 16, 48, 18]);
    assert_eq!(&got.as_samples()[12..16], &[105, 19, 53, 21]);
}

#[test]
fn overlay_composites_at_sixteen_bit_across_mixed_depths() {
    // An `iovl` may composite sub-items of different coded depths — only `grid` requires
    // uniformity. Normalizing every sub-item to the full 16-bit range is what makes that work.
    let a = Item {
        hidden: true,
        ..coded_item(2, 3, 8, 50, 2, 2, vec![colr(0, true)])
    };
    let b = Item {
        hidden: true,
        ..coded_item(3, 3, 10, 130, 2, 2, vec![colr(0, true)])
    };
    let ov = Item {
        references: vec![dimg(&[2, 3])],
        payload: ImageOverlay {
            // On the 16-bit surface the fill channels are used verbatim, not shifted down.
            canvas_fill_value: [0x1234, 0x5678, 0x9ABC, 0xFFFF],
            output_width: 4,
            output_height: 4,
            offsets: vec![(-1, -1), (1, 1)],
        }
        .to_bytes()
        .unwrap(),
        ..base_item(1, *b"iovl")
    };
    let bytes = file(1, vec![ov, a, b]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let got = container
        .image()
        .decode_item_rgba16(1, &mut Mock::default())
        .unwrap();
    assert_eq!((got.width(), got.height()), (4, 4));

    // Both sub-items are opaque, so each simply overwrites where it lands; everywhere else keeps
    // the verbatim fill.
    let widen8 = |s: u16| s * 257;
    let widen10 = |s: u16| ((u64::from(s) * 65535 + 511) / 1023) as u16;
    let samples = got.as_samples();
    for y in 0..4u32 {
        for x in 0..4u32 {
            let o = ((y * 4 + x) * 4) as usize;
            let want = if (x, y) == (0, 0) {
                // A's pixel (1,1), 8-bit identity/GBR.
                [
                    widen8(ecr(50, 1, 1, 8)),
                    widen8(ey(50, 1, 1, 8)),
                    widen8(ecb(50, 1, 1, 8)),
                    65535,
                ]
            } else if (1..3).contains(&x) && (1..3).contains(&y) {
                // B's pixel (x-1, y-1), 10-bit identity/GBR.
                let (sx, sy) = (x - 1, y - 1);
                [
                    widen10(ecr(130, sx, sy, 10)),
                    widen10(ey(130, sx, sy, 10)),
                    widen10(ecb(130, sx, sy, 10)),
                    65535,
                ]
            } else {
                [0x1234, 0x5678, 0x9ABC, 0xFFFF]
            };
            assert_eq!(&samples[o..o + 4], &want, "({x},{y})");
        }
    }
}

// ---- alpha -----------------------------------------------------------------------------------

fn alpha_aux(id: u32, master: u32, base: u8, w: u32, h: u32) -> Item {
    alpha_aux_bd(id, master, base, w, h, 8)
}

/// An alpha auxiliary at an explicit bit depth (monochrome), for testing the >8-bit rescale.
fn alpha_aux_bd(id: u32, master: u32, base: u8, w: u32, h: u32, bd: u8) -> Item {
    let auxc = Property {
        essential: false,
        kind: PropertyKind::AuxiliaryType {
            aux_type: "urn:mpeg:hevc:2015:auxid:1".to_string(),
            aux_subtype: vec![],
        },
    };
    Item {
        hidden: true,
        references: vec![ItemReference {
            reference_type: *b"auxl",
            to_item_ids: vec![master],
        }],
        ..coded_item(id, 0, bd, base, w, h, vec![auxc])
    }
}

#[test]
fn alpha_dimension_mismatch_one_axis_at_a_time() {
    // A width-only (and separately height-only) mismatch must still be rejected: the alpha-dimension
    // guard ORs the two axis checks, so an `||`->`&&` mutation (which would require *both* axes to
    // differ) is caught by a single-axis mismatch.
    let master = || coded_item(1, 3, 8, 33, 2, 2, vec![colr(0, true)]);
    for bad in [alpha_aux(2, 1, 77, 3, 2), alpha_aux(2, 1, 77, 2, 3)] {
        let bytes = file(1, vec![master(), bad]);
        let container = HeifContainer::parse(&bytes).unwrap();
        assert!(matches!(
            container.image().decode_item_rgba8(1, &mut Mock::default()),
            Err(error) if error.kind() == ErrorKind::InvalidInput
        ));
    }
}

#[test]
fn alpha_nine_bit_rescale_is_golden() {
    // A 9-bit alpha auxiliary exercises the depth-rescale arithmetic `(s*255 + max/2) / max` with
    // `max = 511` — where an 8-bit auxiliary is the identity and masks every operator. The samples
    // (255, 258, 272, 275 from the mock gradient) are chosen so every operator mutation provably
    // diverges; in particular `max` itself must be exactly `(1 << 9) - 1 = 511`: with the `- -> /`
    // mutant (`max = 512`) sample 258 yields 66046/512 = 128, and with `- -> +` (`max = 513`)
    // 66046/513 = 128 — both off the correct (65790 + 255)/511 = 129. The round-half term is pinned
    // too: `/ -> %` (`max % 2 = 1`) turns sample 272 into 69361/511 = 135 instead of 136.
    let master = coded_item(1, 3, 8, 33, 2, 2, vec![colr(0, true)]);
    let alpha = alpha_aux_bd(2, 1, 255, 2, 2, 9);
    let bytes = file(1, vec![master, alpha]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let rgba = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    let base = identity_base_rgba(33, 2, 2);
    // Expected 8-bit alpha = (s*255 + 255) / 511 for s = ey(255, x, y, 9) = [255, 258, 272, 275].
    let want_alpha = [127u8, 129, 136, 137];
    for i in 0..4usize {
        assert_eq!(
            &rgba.as_samples()[i * 4..i * 4 + 3],
            &base[i * 4..i * 4 + 3]
        );
        assert_eq!(rgba.as_samples()[i * 4 + 3], want_alpha[i], "alpha px {i}");
    }
}

#[test]
fn alpha_auxiliary_merges_as_gradient() {
    let master = coded_item(1, 3, 8, 33, 2, 2, vec![colr(0, true)]);
    let bytes = file(1, vec![master, alpha_aux(2, 1, 77, 2, 2)]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let rgba = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    let base = identity_base_rgba(33, 2, 2);
    for y in 0..2u32 {
        for x in 0..2u32 {
            let o = ((y * 2 + x) * 4) as usize;
            assert_eq!(rgba.as_samples()[o], base[o]); // colour untouched
            assert_eq!(rgba.as_samples()[o + 1], base[o + 1]);
            assert_eq!(rgba.as_samples()[o + 2], base[o + 2]);
            // Alpha is the monochrome auxiliary's luma gradient (8-bit, no expansion).
            assert_eq!(rgba.as_samples()[o + 3], ey(77, x, y, 8) as u8);
        }
    }
}

#[test]
fn alpha_dimension_mismatch_is_invalid() {
    let master = coded_item(1, 3, 8, 33, 2, 2, vec![colr(0, true)]);
    let bytes = file(1, vec![master, alpha_aux(2, 1, 77, 3, 3)]); // 3x3 alpha for a 2x2 master
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        container.image().decode_item_rgba8(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

#[test]
fn absent_alpha_is_opaque() {
    let item = coded_item(1, 3, 8, 33, 2, 2, vec![colr(0, true)]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let rgba = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    assert!(
        rgba.as_samples()
            .as_chunks::<4>()
            .0
            .iter()
            .all(|px| px[3] == 255)
    );
}

#[test]
fn premultiplied_flag_is_queryable() {
    let master = Item {
        references: vec![ItemReference {
            reference_type: *b"prem",
            to_item_ids: vec![2],
        }],
        ..coded_item(1, 3, 8, 33, 2, 2, vec![colr(0, true)])
    };
    let bytes = file(1, vec![master, alpha_aux(2, 1, 77, 2, 2)]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(container.image().is_premultiplied(1));
    // Decoding still succeeds and does not silently un-premultiply (alpha still the gradient).
    let rgba = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    assert_eq!(rgba.as_samples()[3], ey(77, 0, 0, 8) as u8);
}

// ---- overlay (iovl) --------------------------------------------------------------------------

#[test]
fn overlay_via_planar_is_unsupported() {
    let a = Item {
        hidden: true,
        ..coded_item(2, 3, 8, 10, 2, 2, vec![colr(0, true)])
    };
    let ov = Item {
        references: vec![dimg(&[2])],
        payload: ImageOverlay {
            canvas_fill_value: [0; 4],
            output_width: 4,
            output_height: 4,
            offsets: vec![(0, 0)],
        }
        .to_bytes()
        .unwrap(),
        ..base_item(1, *b"iovl")
    };
    let bytes = file(1, vec![ov, a]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        container
            .image()
            .decode_item_planar(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
}

#[test]
fn overlay_composites_with_fill_clipping_and_alpha() {
    // Canvas 4x4, fill (10,20,30,255). Input A (opaque, id 2) at (-1,-1): only its (1,1) pixel lands
    // at canvas (0,0). Input B (id 3) has a monochrome alpha auxiliary (id 4) at (1,1), so it blends
    // over the fill.
    let a = Item {
        hidden: true,
        ..coded_item(2, 3, 8, 50, 2, 2, vec![colr(0, true)])
    };
    let b = Item {
        hidden: true,
        ..coded_item(3, 3, 8, 130, 2, 2, vec![colr(0, true)])
    };
    let b_alpha = alpha_aux(4, 3, 90, 2, 2);
    let ov = Item {
        references: vec![dimg(&[2, 3])],
        payload: ImageOverlay {
            canvas_fill_value: [0x0A00, 0x1400, 0x1E00, 0xFF00], // (10, 20, 30, 255) after >> 8
            output_width: 4,
            output_height: 4,
            offsets: vec![(-1, -1), (1, 1)],
        }
        .to_bytes()
        .unwrap(),
        ..base_item(1, *b"iovl")
    };
    let bytes = file(1, vec![ov, a, b, b_alpha]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let got = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    assert_eq!((got.width(), got.height()), (4, 4));

    // Independent float reference: fill, then A over, then B over (source-over, unassociated alpha).
    let a_rgba = identity_base_rgba(50, 2, 2); // opaque
    let mut b_rgba = identity_base_rgba(130, 2, 2);
    for i in 0..4usize {
        b_rgba[i * 4 + 3] = ey(90, (i % 2) as u32, (i / 2) as u32, 8) as u8;
    }
    let mut want = vec![0f64; 4 * 4 * 4];
    for px in want.as_chunks_mut::<4>().0 {
        px.copy_from_slice(&[10.0, 20.0, 30.0, 255.0]);
    }
    for (src, (ox, oy)) in [(&a_rgba, (-1i32, -1i32)), (&b_rgba, (1, 1))] {
        for sy in 0..2i32 {
            for sx in 0..2i32 {
                let (x, y) = (ox + sx, oy + sy);
                if x < 0 || y < 0 || x >= 4 || y >= 4 {
                    continue;
                }
                let si = ((sy * 2 + sx) * 4) as usize;
                let di = ((y * 4 + x) * 4) as usize;
                let sa = f64::from(src[si + 3]) / 255.0;
                let da = want[di + 3] / 255.0;
                let out_a = sa + da * (1.0 - sa);
                for c in 0..3 {
                    let sc = f64::from(src[si + c]);
                    let dc = want[di + c];
                    want[di + c] = if out_a > 0.0 {
                        (sc * sa + dc * da * (1.0 - sa)) / out_a
                    } else {
                        0.0
                    };
                }
                want[di + 3] = out_a * 255.0;
            }
        }
    }
    for (g, w) in got.as_samples().iter().zip(&want) {
        assert!((f64::from(*g) - w).abs() <= 1.0, "got {g} want {w}");
    }

    // Literal placement anchors: canvas (0,0) is A's opaque pixel (1,1); canvas (3,0) is fill.
    let a_pixel_11 = &identity_base_rgba(50, 2, 2)[12..16]; // pixel index 1*2+1 = 3
    assert_eq!(&got.as_samples()[0..4], a_pixel_11);
    assert_eq!(&got.as_samples()[12..16], &[10, 20, 30, 255]); // canvas pixel index 0*4+3 = 3
}

// ---- derivation cycles & depth ---------------------------------------------------------------

#[test]
fn mutual_dimg_cycle_errors_without_overflow() {
    // 1 (iden) → 2 (iden) → 1 …
    let a = Item {
        references: vec![dimg(&[2])],
        ..base_item(1, *b"iden")
    };
    let b = Item {
        hidden: true,
        references: vec![dimg(&[1])],
        ..base_item(2, *b"iden")
    };
    let bytes = file(1, vec![a, b]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        container
            .image()
            .decode_item_planar(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::InvalidInput
    ));
}

#[test]
fn derivation_depth_is_bounded() {
    // A chain 1→2→…→8 of idens onto a coded leaf 9 exceeds the depth limit; a shallow chain works.
    let mut items = Vec::new();
    for id in 1..=8u32 {
        items.push(Item {
            hidden: id != 1,
            references: vec![dimg(&[id + 1])],
            ..base_item(id, *b"iden")
        });
    }
    items.push(Item {
        hidden: true,
        ..coded_item(9, 0, 8, 5, 2, 2, vec![])
    });
    let bytes = file(1, items);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(matches!(
        container
            .image()
            .decode_item_planar(1, &mut Mock::default()),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));

    // A 2-deep iden chain onto the same leaf decodes fine.
    let i1 = Item {
        references: vec![dimg(&[2])],
        ..base_item(1, *b"iden")
    };
    let i2 = Item {
        hidden: true,
        references: vec![dimg(&[3])],
        ..base_item(2, *b"iden")
    };
    let leaf = Item {
        hidden: true,
        ..coded_item(3, 0, 8, 5, 2, 2, vec![])
    };
    let bytes = file(1, vec![i1, i2, leaf]);
    let container = HeifContainer::parse(&bytes).unwrap();
    assert!(
        container
            .image()
            .decode_item_planar(1, &mut Mock::default())
            .is_ok()
    );
}

// ---- grid: output-dimension guard & the monochrome/colour split ------------------------------

/// A hidden 4:4:4 colour tile of the given base at 2x2.
fn color_tile(id: u32, base: u8) -> Item {
    Item {
        hidden: true,
        ..coded_item(id, 3, 8, base, 2, 2, vec![])
    }
}

#[test]
fn grid_exact_fit_colour_assembles_all_planes() {
    // A 1x2 grid of 2x2 4:4:4 tiles whose output *exactly* fills the 4x2 tiled canvas. Two facets are
    // pinned: (1) the output-vs-canvas comparisons are `>` (strict) — an exact fit must be accepted,
    // killing the `> -> ==`/`>=` mutants; (2) a *colour* grid takes the non-monochrome assembly
    // branch, so the `chroma == Monochrome` test's `== -> !=` inversion (which would drop the chroma
    // planes and fail `DecodedFrame::new`) is caught by asserting exact Cb/Cr samples.
    let grid = grid_item(1, 1, 2, 4, 2, &[2, 3]);
    let bytes = file(1, vec![grid, color_tile(2, 10), color_tile(3, 40)]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let frame = container
        .image()
        .decode_item_planar(1, &mut Mock::default())
        .unwrap();

    assert_eq!((frame.width(), frame.height()), (4, 2));
    assert_eq!(frame.chroma(), ChromaFormat::Yuv444);
    for oy in 0..2u32 {
        for ox in 0..4u32 {
            let base = if ox < 2 { 10 } else { 40 };
            let (ix, iy) = (ox % 2, oy);
            let i = (oy * 4 + ox) as usize;
            assert_eq!(frame.y()[i], ey(base, ix, iy, 8), "y ({ox},{oy})");
            assert_eq!(frame.cb()[i], ecb(base, ix, iy, 8), "cb ({ox},{oy})");
            assert_eq!(frame.cr()[i], ecr(base, ix, iy, 8), "cr ({ox},{oy})");
        }
    }
}

#[test]
fn grid_output_dimension_guard_rejects_each_violation_individually() {
    // One violated condition at a time for `ow == 0 || oh == 0 || ow > cw || oh > ch` on a 1x2 grid
    // of 2x2 tiles (canvas 4x2), always asserting the guard's *own* message. Because `&&` binds
    // tighter than `||`, each `|| -> &&` mutation fuses one adjacent pair (`a || (b && c) || d`, …),
    // so only a fixture violating exactly that one condition exposes it — the degenerate grid then
    // slips past to a later, different ("zero-sized image") error or an out-of-range tile index.
    for (ow, oh) in [(0u32, 2u32), (4, 0), (5, 2), (4, 3)] {
        let grid = grid_item(1, 1, 2, ow, oh, &[2, 3]);
        let bytes = file(1, vec![grid, mono_tile(2, 10), mono_tile(3, 40)]);
        let container = HeifContainer::parse(&bytes).unwrap();
        let err = container
            .image()
            .decode_item_planar(1, &mut Mock::default())
            .unwrap_err();
        assert!(
            err.to_string().contains("output dimensions exceed"),
            "({ow},{oh}): unexpected error: {err}"
        );
    }
}

// ---- overlay: empty-canvas guard -------------------------------------------------------------

#[test]
fn overlay_zero_height_canvas_is_rejected_by_its_own_guard() {
    // An overlay canvas with a zero height: the `ow != 0 && oh != 0` guard must reject it. The
    // `&& -> ||` mutant lets the degenerate canvas through, failing later with a different
    // ("zero-sized image") error — so the exact guard message pins it.
    let src = Item {
        hidden: true,
        ..coded_item(2, 3, 8, 10, 2, 2, vec![colr(0, true)])
    };
    let ov = Item {
        references: vec![dimg(&[2])],
        payload: ImageOverlay {
            canvas_fill_value: [0; 4],
            output_width: 4,
            output_height: 0,
            offsets: vec![(0, 0)],
        }
        .to_bytes()
        .unwrap(),
        ..base_item(1, *b"iovl")
    };
    let bytes = file(1, vec![ov, src]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let err = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap_err();
    assert!(
        err.to_string().contains("overlay canvas is empty"),
        "unexpected error: {err}"
    );
}

// ---- colour: 4:4:4 chroma indexing -----------------------------------------------------------

#[test]
fn bt601_444_uses_full_chroma_resolution() {
    // A 4:4:4 BT.601 frame: the chroma column index must be `x` (not `x / 2`). Deleting the `Yuv444`
    // match arm falls through to the subsampled `x / 2`, reading the wrong Cb/Cr column — caught by a
    // golden that varies the chroma across adjacent columns.
    let item = coded_item(1, 3, 8, 15, 3, 2, vec![colr(6, false)]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let rgba = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    for y in 0..2u32 {
        for x in 0..3u32 {
            let yv = ey(15, x, y, 8) as u8;
            let cb = ecb(15, x, y, 8) as u8; // 4:4:4: chroma index == luma index
            let cr = ecr(15, x, y, 8) as u8;
            let (r, g, b) = ycbcr_to_rgb(yv, cb, cr, ColorRange::Limited);
            let o = ((y * 3 + x) * 4) as usize;
            assert_eq!(&rgba.as_samples()[o..o + 4], &[r, g, b, 255], "({x},{y})");
        }
    }
}

// ---- clap: per-denominator guards, integer division, and offset arithmetic -------------------

/// Decodes a 4x2 identity image carrying a single `clap`, returning the (expected-`Err`) error.
fn clap_error(clap_prop: Property) -> Error {
    let item = coded_item(1, 3, 8, 0, 4, 2, vec![colr(0, true), clap_prop]);
    let bytes = file(1, vec![item]);
    let container = HeifContainer::parse(&bytes).unwrap();
    container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap_err()
}

#[test]
fn clap_guard_messages_pin_each_disjunction() {
    // Each `clap` validation guard ORs several conditions; a single failing condition must fire that
    // guard's *own* message. Relaxing any `||` to `&&` lets the input slip to a later guard with a
    // different message, so asserting the exact message kills the `||` mutants one guard at a time.
    let neg = |v: i32| v as u32;

    // Zero denominators, one at a time (width_d / height_d / horiz_off_d / vert_off_d): each must
    // fire the zero-denominator guard's own message. Because `&&` binds tighter than `||`, each
    // `|| -> &&` mutation fuses one adjacent pair of the four-way disjunction, so only the fixture
    // zeroing exactly that denominator exposes it — the input then slips to the integer-value guard
    // (a different message) or into a division-by-zero panic in `clap_offset`.
    for zeroed in [
        clap(2, 0, 2, 1, 0, 1, 0, 1),
        clap(2, 1, 2, 0, 0, 1, 0, 1),
        clap(2, 1, 2, 1, 0, 0, 0, 1),
        clap(2, 1, 2, 1, 0, 1, 0, 0),
    ] {
        let e = clap_error(zeroed);
        assert!(e.to_string().contains("zero denominator"), "{e}");
    }

    // Non-integer width only (3/2): the integer-value guard.
    let e = clap_error(clap(3, 2, 2, 1, 0, 1, 0, 1));
    assert!(e.to_string().contains("width/height is not integer"), "{e}");

    // Zero-sized crop (width_n = 0): the zero-crop guard.
    let e = clap_error(clap(0, 1, 2, 1, 0, 1, 0, 1));
    assert!(e.to_string().contains("zero-sized crop"), "{e}");

    // Negative left only (horizOff = -2 ⇒ left = -1): the out-of-bounds guard's `left < 0 || top < 0`
    // disjunction. Under `&&` the negative left slips through and the crop indexes out of bounds.
    let e = clap_error(clap(2, 1, 2, 1, neg(-2), 1, 0, 1));
    assert!(e.to_string().contains("outside the image"), "{e}");

    // Height overshoot only (top = 1, crop_h = 2 on a 2-tall image ⇒ top + crop_h = 3 > 2): pins the
    // `top + crop_h` addition (the `+ -> -`/`*` mutants) and the final `|| top+crop_h>h` term.
    let e = clap_error(clap(2, 1, 2, 1, neg(-1), 1, 1, 1));
    assert!(e.to_string().contains("outside the image"), "{e}");
}

#[test]
fn clap_fractional_denominators_crop_is_golden() {
    // width_n/width_d = 4/2 and height_n/height_d = 4/2 both reduce to a 2x2 crop at (1, 0). The
    // `/ -> *` mutants on the crop-size divisions blow the crop past the image, so the exact cropped
    // pixels pin both integer divisions.
    let base = identity_base_rgba(91, 4, 2);
    let want = ref_crop(&base, 4, 1, 0, 2, 2);
    let (got, gw, gh) = decode_rgba_with_props(91, 4, 2, vec![clap(4, 2, 4, 2, 0, 1, 0, 1)]);
    assert_eq!((gw, gh), (2, 2));
    assert_eq!(got, want);
}

#[test]
fn clap_offset_denominator_is_golden() {
    // A horizontal offset with denominator 2 (horizOff = 0/2) still centres the 2x2 crop at left = 1.
    // The offset numerator `(dim - crop) * off_d` and the denominator `2 * off_d` both multiply by
    // off_d; the `* -> /` mutants make the offset non-integer or out of bounds, so the exact crop
    // pins both multiplications.
    let base = identity_base_rgba(92, 4, 2);
    let want = ref_crop(&base, 4, 1, 0, 2, 2);
    let (got, gw, gh) = decode_rgba_with_props(92, 4, 2, vec![clap(2, 1, 2, 1, 0, 2, 0, 1)]);
    assert_eq!((gw, gh), (2, 2));
    assert_eq!(got, want);
}

// ---- overlay compositing: the fully-transparent branch and source-over rounding --------------

#[test]
fn composite_over_fully_transparent_pixel_clears_in_place() {
    // A transparent source pixel over a transparent (but non-black) canvas produces a fully
    // transparent output pixel, taking the `out_a == 0` fast path that writes `canvas[di..di+4]`.
    // The `+ -> -`/`*` mutants on that slice bound write to the wrong pixel (or panic), so pinning
    // the cleared pixel — and that its neighbour is untouched — kills them.
    let src = Item {
        hidden: true,
        ..coded_item(2, 3, 8, 167, 2, 1, vec![colr(0, true)])
    };
    // Alpha auxiliary base 253 ⇒ alpha (0,0) = 253, alpha (1,0) = (253 + 3) & 0xFF = 0.
    let src_alpha = alpha_aux(3, 2, 253, 2, 1);
    let ov = Item {
        references: vec![dimg(&[2])],
        payload: ImageOverlay {
            canvas_fill_value: [0x0A00, 0x1400, 0x1E00, 0x0000], // (10, 20, 30, 0) after >> 8
            output_width: 2,
            output_height: 1,
            offsets: vec![(0, 0)],
        }
        .to_bytes()
        .unwrap(),
        ..base_item(1, *b"iovl")
    };
    let bytes = file(1, vec![ov, src, src_alpha]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let got = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    // Canvas pixel (1,0) is the transparent source pixel over the transparent fill ⇒ cleared.
    assert_eq!(&got.as_samples()[4..8], &[0, 0, 0, 0]);
}

#[test]
fn composite_over_source_over_rounding_is_golden() {
    // A semi-transparent (alpha 128) source over an opaque black canvas. The channel blend
    // `(num + out_a/2) / out_a` rounds half up; dropping the half (`out_a / 2 -> out_a % 2`) shifts
    // every channel down by one. The exact blended pixel pins the rounding.
    let src = Item {
        hidden: true,
        ..coded_item(2, 3, 8, 167, 1, 1, vec![colr(0, true)]) // identity ⇒ [R=1, G=167, B=207]
    };
    let src_alpha = alpha_aux(3, 2, 128, 1, 1); // alpha 128
    let ov = Item {
        references: vec![dimg(&[2])],
        payload: ImageOverlay {
            canvas_fill_value: [0, 0, 0, 0xFF00], // opaque black
            output_width: 1,
            output_height: 1,
            offsets: vec![(0, 0)],
        }
        .to_bytes()
        .unwrap(),
        ..base_item(1, *b"iovl")
    };
    let bytes = file(1, vec![ov, src, src_alpha]);
    let container = HeifContainer::parse(&bytes).unwrap();
    let got = container
        .image()
        .decode_item_rgba8(1, &mut Mock::default())
        .unwrap();
    assert_eq!(got.as_samples(), &[1, 84, 104, 255]);
}
