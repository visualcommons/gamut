//! Bounded zlib inflation (RFC 1950/1951) behind the optional `inflate` feature — the workspace's
//! one point of contact with [`miniz_oxide`].
//!
//! Every compressed payload a gamut decoder inflates (PNG `IDAT`/`iCCP`/`zTXt`/`iTXt`, TIFF and
//! DNG Deflate strips and tiles) is attacker-controlled, so every inflation carries a hard output
//! cap: a tiny stream claiming to inflate without bound (a "zlib bomb") fails cleanly instead of
//! exhausting memory. Inflating short of the cap is *not* an error here: whether a short stream is
//! acceptable is the caller's format rule, so the length check stays with the caller.

use gamut_core::Error;
use miniz_oxide::inflate::TINFLStatus;

/// Inflates the zlib stream `data`, refusing to produce more than `max_out` bytes.
///
/// `origin` is the Cargo package or format layer calling — the decoder whose input this is — and
/// becomes the returned error's [`Error::origin`], so a failure is attributed to the format that
/// read the stream rather than to this primitive. Pass `env!("CARGO_PKG_NAME")`.
///
/// The output may be shorter than `max_out`; a caller whose format fixes the inflated length
/// checks it.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error, carrying
/// `origin` and miniz's status as its [`Error::detail`]:
///
/// - `"zlib stream inflates past the expected size"` when the stream would produce more than
///   `max_out` bytes — a bomb, or a stream longer than the caller's geometry allows;
/// - `"corrupt zlib stream"` for every other failure: a bad header, a malformed block, a
///   truncated stream or a wrong Adler-32 trailer.
pub fn inflate_zlib(data: &[u8], max_out: usize, origin: &'static str) -> Result<Vec<u8>, Error> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, max_out).map_err(|error| {
        let message = match error.status {
            TINFLStatus::HasMoreOutput => "zlib stream inflates past the expected size",
            _ => "corrupt zlib stream",
        };
        Error::invalid_input(origin, message)
            .with_detail(format!("miniz status {:?}", error.status))
    })
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;
    use crate::DeflateEncoder;

    const ORIGIN: &str = "gamut-test";

    fn zlib(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        DeflateEncoder::new().zlib_compress(payload, &mut out);
        out
    }

    #[test]
    fn round_trips_own_encoder_output() {
        let payload: Vec<u8> = (0..2000u32).map(|i| (i * 7) as u8).collect();
        assert_eq!(
            inflate_zlib(&zlib(&payload), payload.len(), ORIGIN).unwrap(),
            payload
        );
    }

    /// Short output is the caller's check, not this function's: a cap above the stream's length
    /// still succeeds with the stream's own bytes.
    #[test]
    fn output_short_of_the_cap_is_returned_as_is() {
        let payload = [1u8, 2, 3, 4];
        assert_eq!(inflate_zlib(&zlib(&payload), 16, ORIGIN).unwrap(), payload);
    }

    /// A stream inflating to exactly `max_out` is accepted; one byte over is the *cap* error,
    /// distinct from corruption so a caller reports a bomb rather than a damaged file.
    #[test]
    fn caps_output_size() {
        let payload = vec![0u8; 4096];
        let stream = zlib(&payload);
        assert!(inflate_zlib(&stream, payload.len(), ORIGIN).is_ok());
        let error = inflate_zlib(&stream, payload.len() - 1, ORIGIN).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.static_message(),
            Some("zlib stream inflates past the expected size")
        );
        assert_eq!(error.origin(), Some(ORIGIN));
        assert_eq!(error.detail(), Some("miniz status HasMoreOutput"));
    }

    /// Every non-cap failure is the corrupt-stream error, carrying the caller's origin.
    fn assert_corrupt(result: Result<Vec<u8>, Error>) {
        let error = result.expect_err("corrupt input must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.static_message(), Some("corrupt zlib stream"));
        assert_eq!(error.origin(), Some(ORIGIN));
        assert!(
            error
                .detail()
                .is_some_and(|d| d.starts_with("miniz status "))
        );
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert_corrupt(inflate_zlib(&[], 16, ORIGIN));
        assert_corrupt(inflate_zlib(&[0xFF, 0xFF, 0x00], 16, ORIGIN)); // bad zlib header
        let mut stream = zlib(&[1, 2, 3, 4]);
        assert_corrupt(inflate_zlib(&stream[..stream.len() - 1], 16, ORIGIN)); // truncated Adler-32
        let last = stream.len() - 1;
        stream[last] ^= 0xFF;
        assert_corrupt(inflate_zlib(&stream, 16, ORIGIN)); // wrong Adler-32
    }

    /// The bounded-inflate law (`crate::invariants`), sampled here at a pinned seed and searched
    /// unbounded by the fuzz tier's `deflate_inflate_zlib` target.
    mod cap_law {
        use proptest::collection;
        use proptest::prelude::{Just, Strategy, prop_oneof, proptest};
        use proptest::test_runner::{Config, RngSeed};

        use super::zlib;
        use crate::invariants::inflation_stays_within_cap;

        /// The workspace property-test configuration (`docs/testing.md`): pinned seed, bounded
        /// shrinking, no failure persistence.
        fn config() -> Config {
            Config {
                cases: 512,
                max_shrink_iters: 2048,
                max_shrink_time: 10_000,
                rng_seed: RngSeed::Fixed(0x6761_6D75_745F_6466),
                failure_persistence: None,
                ..Config::default()
            }
        }

        /// A stream and a cap: this crate's own zlib output — intact, or with one byte flipped or
        /// the tail cut — so most cases reach a real decode, plus arbitrary bytes for the
        /// header-refusal path. Payloads draw from four symbols so they compress into
        /// back-references, and the cap lands on either side of the payload's length.
        fn stream_and_cap() -> impl Strategy<Value = (Vec<u8>, usize)> {
            let payload = collection::vec(0u8..4, 0..512);
            let encoded = (payload, any_flip(), 0usize..1024).prop_map(|(payload, flip, cap)| {
                let mut stream = zlib(&payload);
                match flip {
                    Damage::None => {}
                    Damage::Flip(at, mask) => {
                        let at = at % stream.len();
                        stream[at] ^= mask;
                    }
                    Damage::Cut(keep) => stream.truncate(keep % stream.len()),
                }
                (stream, cap)
            });
            let arbitrary = (collection::vec(proptest::num::u8::ANY, 0..64), 0usize..1024);
            prop_oneof![3 => encoded, 1 => arbitrary]
        }

        #[derive(Debug, Clone)]
        enum Damage {
            None,
            Flip(usize, u8),
            Cut(usize),
        }

        fn any_flip() -> impl Strategy<Value = Damage> {
            prop_oneof![
                Just(Damage::None),
                (proptest::num::usize::ANY, 1u8..=255)
                    .prop_map(|(at, mask)| Damage::Flip(at, mask)),
                proptest::num::usize::ANY.prop_map(Damage::Cut),
            ]
        }

        proptest! {
            #![proptest_config(config())]

            #[test]
            fn inflation_stays_within_cap_holds((stream, cap) in stream_and_cap()) {
                if let Err(violation) = inflation_stays_within_cap(&stream, cap) {
                    panic!("{violation}");
                }
            }
        }
    }
}
