//! The decoder rejects inconsistent metadata and honours the photometric interpretation, exercising
//! branches the well-formed oracle round-trips never reach (P10 / #110).
//!
//! These patch a single tag of an otherwise-valid file **in place** (preserving the strip pixel
//! data the decoder reads) so a mutated validation either decodes a wrong-but-`Ok` image, panics, or
//! flips the colours — all distinguishable from the correct clean rejection.

use gamut_core::{Bilevel, DecodeImage, Dimensions, EncodeImage, Gray8, ImageBuf, ImageRef, Rgb8};
use gamut_tiff::{ByteOrder, Compression, Ifd, TiffDecoder, TiffEncoder, Value, Variant, tags};

mod common;
use common::write_image;

fn valid_rgb(w: u32, h: u32) -> Vec<u8> {
    let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 7) as u8).collect();
    let mut out = Vec::new();
    TiffEncoder::new()
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb, Dimensions::new(w, h).unwrap()).unwrap(),
            &mut out,
        )
        .unwrap();
    out
}

/// Overwrites the inline value of `tag` on the first IFD (little-endian classic TIFF), preserving
/// every strip/tile byte. Handles `SHORT` (type 3) and `LONG` (type 4) inline scalars.
fn patch_inline(tiff: &mut [u8], tag: u16, new: u32) {
    let ifd = u32::from_le_bytes(tiff[4..8].try_into().unwrap()) as usize;
    let n = u16::from_le_bytes(tiff[ifd..ifd + 2].try_into().unwrap()) as usize;
    for e in 0..n {
        let p = ifd + 2 + e * 12;
        if u16::from_le_bytes(tiff[p..p + 2].try_into().unwrap()) == tag {
            match u16::from_le_bytes(tiff[p + 2..p + 4].try_into().unwrap()) {
                3 => tiff[p + 8..p + 10].copy_from_slice(&(new as u16).to_le_bytes()),
                4 => tiff[p + 8..p + 12].copy_from_slice(&new.to_le_bytes()),
                ty => panic!("tag {tag} has non-inline type {ty}"),
            }
            return;
        }
    }
    panic!("tag {tag} not found");
}

/// The decoder's static message for a file it rejects, for tests that must distinguish *which*
/// guard fired rather than merely that one did.
fn err_message(tiff: &[u8]) -> Option<&'static str> {
    match TiffDecoder::new().decode_page(tiff, 0) {
        Ok(_) => panic!("the decoder must reject this file"),
        Err(error) => error.static_message(),
    }
}

fn errs(tiff: &[u8]) -> bool {
    // The correct decoder rejects these cleanly (Err, no panic). A mutated guard that lets the file
    // through then trips a later bounds/zero check and panics — which fails this test, so the mutant
    // is caught either way.
    TiffDecoder::new().decode_page(tiff, 0).is_err()
}

#[test]
fn rejects_zero_height() {
    // height 0 (width non-zero) must be rejected; both `width == 0` and `height == 0` are real,
    // so the guard cannot collapse to a single conjunction.
    let mut v = valid_rgb(8, 8);
    patch_inline(&mut v, tags::IMAGE_LENGTH, 0);
    assert!(errs(&v));
}

#[test]
fn rejects_inconsistent_sample_count() {
    // SamplesPerPixel says 1 but BitsPerSample still lists three entries: the `bits.len() != spp`
    // check must fire. Also flip the photometric to BlackIsZero so that, if a mutated `&&` lets the
    // file through, it forms a *valid* (1, 8, gray) layout and decodes a wrong-but-`Ok` image — which
    // the rejection check above must instead refuse.
    let mut v = valid_rgb(8, 8);
    patch_inline(&mut v, tags::SAMPLES_PER_PIXEL, 1);
    patch_inline(&mut v, tags::PHOTOMETRIC_INTERPRETATION, 1); // -> BlackIsZero (valid for 1 sample)
    assert!(TiffDecoder::new().decode_page(&v, 0).is_err());
}

#[test]
fn rejects_zero_tile_dimension() {
    let rgb: Vec<u8> = (0..32 * 32 * 3).map(|i| (i * 7) as u8).collect();
    let mut v = Vec::new();
    TiffEncoder::new()
        .with_tiling(16, 16)
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb, Dimensions::new(32, 32).unwrap()).unwrap(),
            &mut v,
        )
        .unwrap();
    patch_inline(&mut v, tags::TILE_WIDTH, 0);
    assert!(errs(&v));
}

#[test]
fn honours_white_is_zero_photometric() {
    // gamut writes bilevel as BlackIsZero (1). Flip PhotometricInterpretation to WhiteIsZero (0) in
    // place; the same stored bits must now decode to the inverted image — pinning the `bit == 0` test
    // on the WhiteIsZero branch the encoder itself never produces.
    let (w, h) = (13u32, 9u32);
    let src: Vec<u8> = (0..w * h)
        .map(|i| if i % 3 == 0 { 255 } else { 0 })
        .collect();
    let mut tiff = Vec::new();
    TiffEncoder::new()
        .with_compression(Compression::None)
        .encode_image(
            ImageRef::<Bilevel>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
            &mut tiff,
        )
        .unwrap();
    patch_inline(&mut tiff, tags::PHOTOMETRIC_INTERPRETATION, 0); // BlackIsZero -> WhiteIsZero
    let got: ImageBuf<Gray8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
    let inverted: Vec<u8> = src.iter().map(|&v| 255 - v).collect();
    assert_eq!(got.as_samples(), inverted.as_slice());
}

/// Increments the element count of an out-of-line array tag by 1, leaving the (now one-short) array
/// data in place. The decoder reads one extra, ignored, element — so the only effect is that the
/// tag's count no longer equals the strip/tile count, which the consistency check must reject.
fn bump_count(tiff: &mut [u8], tag: u16) {
    let ifd = u32::from_le_bytes(tiff[4..8].try_into().unwrap()) as usize;
    let n = u16::from_le_bytes(tiff[ifd..ifd + 2].try_into().unwrap()) as usize;
    for e in 0..n {
        let p = ifd + 2 + e * 12;
        if u16::from_le_bytes(tiff[p..p + 2].try_into().unwrap()) == tag {
            let c = u32::from_le_bytes(tiff[p + 4..p + 8].try_into().unwrap());
            tiff[p + 4..p + 8].copy_from_slice(&(c + 1).to_le_bytes());
            return;
        }
    }
    panic!("tag {tag} not found");
}

#[test]
fn rejects_strip_count_mismatch() {
    // A tall image is split into several strips. One extra (ignored) StripOffsets entry makes
    // offsets.len() != strips while StripByteCounts.len() still equals it — so the guard must fire on
    // the offset disjunct alone (a mutated `&&` would decode the image fine and accept it).
    let mut v = valid_rgb(200, 100);
    bump_count(&mut v, tags::STRIP_OFFSETS);
    assert!(TiffDecoder::new().decode_page(&v, 0).is_err());
}

#[test]
fn rejects_an_image_past_the_size_cap() {
    // 8000x8000 RGB declares 192 MB of stored samples against a 64 MiB cap. The assertion is on the
    // *message*, not merely `is_err()`: with the guard removed the file is still rejected, but by the
    // strip-count check further down — so only the message distinguishes the guard doing its job.
    let mut v = valid_rgb(8, 8);
    patch_inline(&mut v, tags::IMAGE_WIDTH, 8000);
    patch_inline(&mut v, tags::IMAGE_LENGTH, 8000);
    assert_eq!(err_message(&v), Some("TIFF: image exceeds the size limit"));
}

#[test]
fn rejects_a_tile_past_the_size_cap() {
    // One 16x16 tile covering the whole image, so widening the tile to 8192x8192 keeps the tile count
    // at 1 (the count check would otherwise fire first) while the tile itself declares 192 MB.
    // Uncompressed on purpose: `Compression::None` slices rather than reserving, so a guard-removed
    // run fails on the short block instead of attempting the allocation — again, only the message
    // separates the two.
    let rgb: Vec<u8> = (0..16 * 16 * 3).map(|i| (i * 7) as u8).collect();
    let mut v = Vec::new();
    TiffEncoder::new()
        .with_compression(Compression::None)
        .with_tiling(16, 16)
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb, Dimensions::new(16, 16).unwrap()).unwrap(),
            &mut v,
        )
        .unwrap();
    patch_inline(&mut v, tags::TILE_WIDTH, 8192);
    patch_inline(&mut v, tags::TILE_LENGTH, 8192);
    assert_eq!(err_message(&v), Some("TIFF: tile exceeds the size limit"));
}

/// A structurally valid single-strip grayscale TIFF declaring `bits` bits per sample and, when
/// `sample_format` is `Some`, that tag. The strip really does hold `width * height * bits / 8`
/// bytes, so nothing but the declared format/depth can be what the decoder objects to.
///
/// Hand-built rather than encoder-produced because `patch_inline` can only rewrite a tag that is
/// already present, and `SampleFormat` is one the encoder has no reason to write.
fn declared_page(width: u32, height: u32, bits: u16, sample_format: Option<u16>) -> Vec<u8> {
    let mut ifd = Ifd::new();
    ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![width as u16]));
    ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![height as u16]));
    ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![bits]));
    ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
    ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![1]));
    ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
    ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![height as u16]));
    if let Some(format) = sample_format {
        ifd.set(tags::SAMPLE_FORMAT, Value::Short(vec![format]));
    }
    let strip = vec![0u8; (width * height) as usize * (bits as usize / 8).max(1)];
    write_image(ByteOrder::LittleEndian, Variant::Classic, &ifd, &[strip]).expect("write fixture")
}

#[test]
fn rejects_floating_point_samples() {
    // Both the 32-bit float a float TIFF actually uses, and the 16-bit half-float that is the real
    // trap: `bps = 16` clears every depth gate, so only the format check stands between it and a
    // silent misdecode as unsigned. Both must name the format, not the depth.
    for bits in [16u16, 32] {
        assert_eq!(
            err_message(&declared_page(8, 4, bits, Some(3))),
            Some("TIFF: floating-point samples not supported"),
            "{bits}-bit float must be refused by format"
        );
    }
}

#[test]
fn rejects_signed_integer_samples() {
    assert_eq!(
        err_message(&declared_page(8, 4, 16, Some(2))),
        Some("TIFF: signed-integer samples not supported")
    );
}

#[test]
fn rejects_undefined_sample_format() {
    assert_eq!(
        err_message(&declared_page(8, 4, 8, Some(4))),
        Some("TIFF: undefined sample format not supported")
    );
}

#[test]
fn rejects_unrecognised_sample_format() {
    // `0` is not a registered code. Refused rather than defaulted to unsigned — guessing here is
    // the silent misdecode the typed error exists to prevent.
    assert_eq!(
        err_message(&declared_page(8, 4, 8, Some(0))),
        Some("TIFF: unrecognised SampleFormat tag value")
    );
}

#[test]
fn rejects_mixed_sample_formats() {
    // Three samples that disagree: one value cannot describe the page, so it is refused rather
    // than silently taking the first.
    let mut ifd = Ifd::new();
    ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![8]));
    ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![4]));
    ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8, 8, 8]));
    ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![3]));
    ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![2]));
    ifd.set(tags::COMPRESSION, Value::Short(vec![1]));
    ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![4]));
    ifd.set(tags::SAMPLE_FORMAT, Value::Short(vec![1, 3, 1]));
    let tiff = write_image(
        ByteOrder::LittleEndian,
        Variant::Classic,
        &ifd,
        &[vec![0u8; 8 * 4 * 3]],
    )
    .expect("write fixture");
    assert_eq!(
        err_message(&tiff),
        Some("TIFF: mixed sample formats not supported")
    );
}

#[test]
fn rejects_thirty_two_bit_samples() {
    // Unsigned 32-bit: the format is fine, the depth is not. The message must name the depth —
    // before this gate moved above the photometric table, this file was reported as an unsupported
    // photometric/sample combination, which pointed at the wrong tag entirely.
    assert_eq!(
        err_message(&declared_page(8, 4, 32, Some(1))),
        Some("TIFF: 32-bit samples not supported")
    );
}

#[test]
fn rejects_an_unsupported_bit_depth() {
    // 4-bit grayscale is a real TIFF layout this crate has not implemented; it must be refused by
    // depth rather than falling through to a colour-mode complaint.
    assert_eq!(
        err_message(&declared_page(8, 4, 4, None)),
        Some("TIFF: unsupported bits per sample")
    );
}

#[test]
fn absent_sample_format_defaults_to_unsigned() {
    // The TIFF 6.0 default. An 8-bit page with no SampleFormat tag must decode, not be refused —
    // pinning that the format check cannot reject the overwhelmingly common case.
    assert!(
        TiffDecoder::new()
            .decode_page(&declared_page(8, 4, 8, None), 0)
            .is_ok()
    );
}

#[test]
fn rejects_tile_count_mismatch() {
    let rgb: Vec<u8> = (0..32 * 32 * 3).map(|i| (i * 7) as u8).collect();
    let mut v = Vec::new();
    TiffEncoder::new()
        .with_tiling(16, 16)
        .encode_image(
            ImageRef::<Rgb8>::new(&rgb, Dimensions::new(32, 32).unwrap()).unwrap(),
            &mut v,
        )
        .unwrap();
    bump_count(&mut v, tags::TILE_OFFSETS);
    assert!(TiffDecoder::new().decode_page(&v, 0).is_err());
}
