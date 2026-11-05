//! Exercises `gamut-core` strictly through its public API, the way a downstream codec crate does.
//! Nothing here can reach a non-`pub` item, so this file also guards against the surface
//! accidentally dropping an export or a trait method a real consumer needs. The inline unit tests
//! cover each type's own contract; this covers the *public composition* — encode then decode,
//! end to end.

use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, Error, ErrorKind, ImageBuf, ImageRef, Pixel, Result, Rgb8,
};

/// A minimal self-describing codec: a big-endian `width`,`height` header followed by the raw
/// interleaved samples. Just enough that decoding has to reconstruct [`Dimensions`] from the stream
/// and re-validate the payload, so the round-trip touches the whole public buffer contract.
struct RawCodec;

impl EncodeImage<Rgb8> for RawCodec {
    fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
        let start = out.len();
        out.extend_from_slice(&image.width().to_be_bytes());
        out.extend_from_slice(&image.height().to_be_bytes());
        out.extend_from_slice(image.as_samples());
        Ok(out.len() - start)
    }
}

impl DecodeImage<Rgb8> for RawCodec {
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb8>> {
        let header = data
            .get(..8)
            .ok_or_else(|| Error::invalid_input("raw", "truncated header"))?;
        let width = u32::from_be_bytes(header[0..4].try_into().expect("8-byte header"));
        let height = u32::from_be_bytes(header[4..8].try_into().expect("8-byte header"));
        let dims = Dimensions::new(width, height)?;
        // `ImageBuf::new` re-validates that the payload length matches the decoded dimensions.
        ImageBuf::new(data[8..].to_vec(), dims)
    }
}

#[test]
fn round_trips_through_the_public_api() {
    let dims = Dimensions::new(3, 2).unwrap();
    let samples: Vec<u8> = (0..dims.sample_count(Rgb8::CHANNELS).unwrap() as u8).collect();
    let original = ImageBuf::<Rgb8>::new(samples, dims).unwrap();

    // `encode_to_vec` is the provided default over `encode_image`; `as_ref` borrows the owned image.
    let bytes = RawCodec.encode_to_vec(original.as_ref()).unwrap();
    let decoded = RawCodec.decode_image(&bytes).unwrap();

    // Public `PartialEq` compares dimensions and samples together.
    assert_eq!(decoded, original);
    assert_eq!(decoded.dimensions(), dims);
}

#[test]
fn decode_rejects_a_truncated_stream() {
    // A well-formed 3x2 RGB stream (18 payload samples)...
    let full = RawCodec
        .encode_to_vec(
            ImageBuf::<Rgb8>::new(vec![0u8; 18], Dimensions::new(3, 2).unwrap())
                .unwrap()
                .as_ref(),
        )
        .unwrap();
    // ...with the header intact but most of the payload lost is rejected, not silently padded.
    let truncated = &full[..8 + 2];
    let err = RawCodec.decode_image(truncated).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(err.origin(), Some("gamut-core"));
}
