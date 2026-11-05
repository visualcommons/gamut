//! Pluggable **IDAT zlib backends** — the PNG codestream seam (issue #278, under the
//! codestream-IoC umbrella #241).
//!
//! # Why PNG has this seam
//!
//! A PNG file is a container of typed chunks around one compressed payload: the concatenation of
//! every IDAT chunk is a single **zlib stream** (RFC 1950 over RFC 1951 DEFLATE). That stream *is*
//! the PNG codestream, and it dominates decode time. Unlike the TIFF/DNG Deflate paths — which are
//! deliberately left as software-only — DEFLATE has genuine hardware offload (Intel QAT and IAA,
//! IBM zEDC, POWER nx-gzip) as well as much faster software implementations (zlib-ng, libdeflate,
//! ISA-L). So PNG is **in scope** for #241: this module lets a caller route the IDAT stream through
//! any of them without forking the crate.
//!
//! Out of the box the choice is compile-time: encode goes through the in-house [`gamut_deflate`]
//! encoder (zopfli-class), decode through [`gamut_deflate::inflate_zlib`], its bounded wrapper over
//! `miniz_oxide`. Both stay as the implicit *tail* of the registry; nothing changes for a caller
//! that pushes no backend.
//!
//! # What crosses the seam
//!
//! Exactly the concatenated-IDAT zlib stream, and nothing else. Scanline **filtering** (§9) and
//! sub-byte **packing** are PNG *structure*, not compression, so they stay crate-side: a backend
//! sees the filtered byte stream on the encode side and produces it on the decode side. This keeps
//! a backend to one job it can actually accelerate.
//!
//! # PNG-local by design
//!
//! [`ZlibInflater`] / [`ZlibDeflater`] are **PNG's own traits**, not a shared `gamut-deflate`
//! abstraction. `gamut-deflate` is unchanged by this module and gains no plugin surface, and the
//! TIFF/DNG Deflate code paths are deliberately **not** routed through this seam. The datum here
//! carries PNG context ([`IdatInfo`]) that a general-purpose DEFLATE abstraction has no business
//! knowing, and PNG's security posture (see below) is PNG's to enforce.
//!
//! # The fallback contract (#241)
//!
//! Identical in both directions and to every other gamut format crate, and implemented once, by
//! [`gamut_codec_abi::Registry`]:
//!
//! 1. Backends are tried in **push order**.
//! 2. [`supports`](ZlibInflater::supports) returning `false` is the *only* signal that falls
//!    through to the next backend.
//! 3. A backend that **accepts** and then **fails** — with any error, an
//!    [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) one included — propagates its
//!    error to the caller. The host does not retry a later backend or the built-in tail, because a
//!    partially-produced result must never be silently masked.
//! 4. The built-in implementation is the implicit **tail**, tried last: [`gamut_deflate`] for
//!    encode, [`gamut_deflate::inflate_zlib`] for decode. A different inflater behind that
//!    function would slot into the decode tail with no change to this surface.
//!
//! `Send` is not a supertrait of either trait: the encoder and decoder bound it where they insert
//! a backend (`push_backend`), because they hold their registry behind `Arc<Mutex<…>>`.
//!
//! # Security invariant: the host owns the zlib-bomb cap
//!
//! PNG input is attacker-controlled and a tiny IDAT can claim to inflate without bound. The output
//! cap is therefore enforced **by the host, not by the backend**:
//!
//! - `max_out` is passed to [`ZlibInflater::inflate`] so a cooperative backend can stop early, and
//! - the host **re-checks the returned length** afterwards. A backend that returns more than
//!   `max_out` bytes is rejected with an
//!   [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error — never truncated,
//!   never trusted.
//!
//! A third-party or FFI backend is untrusted code; the limit survives a backend that ignores or
//! misimplements it.
//!
//! # Example
//!
//! ```
//! use gamut_core::Result;
//! use gamut_png::{IdatInfo, PngEncoder, ZlibDeflater};
//!
//! /// A deflater that only handles 8-bit images.
//! struct EightBitOnly;
//! impl ZlibDeflater for EightBitOnly {
//!     fn supports(&mut self, info: &IdatInfo) -> bool {
//!         info.bit_depth() == 8
//!     }
//!     fn deflate(&mut self, _info: &IdatInfo, raw: &[u8]) -> Result<Vec<u8>> {
//!         let mut zlib = Vec::new();
//!         gamut_deflate::DeflateEncoder::new().zlib_compress(raw, &mut zlib);
//!         Ok(zlib)
//!     }
//! }
//!
//! let mut encoder = PngEncoder::new();
//! encoder.push_backend(EightBitOnly);
//! ```

use gamut_codec_abi::{Registry, RegistryError};
use gamut_core::{Error, Result};

use crate::color::ColorType;

/// The PNG context a backend is offered alongside the IDAT zlib stream.
///
/// Every field describes the image the stream belongs to, so a backend can decide whether it wants
/// the job (say, a hardware queue that is only worth its setup cost above some size) without
/// re-parsing the PNG. The values come from IHDR (§11.2.1) on decode and from the encoder's chosen
/// output encoding on encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdatInfo {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: ColorType,
    raw_len: usize,
}

impl IdatInfo {
    /// Builds the descriptor. `raw_len` is the length of the **filtered** byte stream — the
    /// inflated form — which for PNG is always known ahead of time from IHDR.
    ///
    /// The codec normally builds this itself; it is public so a backend author can construct one
    /// to unit-test a [`ZlibInflater`] / [`ZlibDeflater`] implementation directly.
    #[must_use]
    pub fn new(
        width: u32,
        height: u32,
        bit_depth: u8,
        color_type: ColorType,
        raw_len: usize,
    ) -> Self {
        Self {
            width,
            height,
            bit_depth,
            color_type,
            raw_len,
        }
    }

    /// Image width in pixels (IHDR).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels (IHDR).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bits per sample: `1`, `2`, `4`, `8`, or `16` (IHDR).
    #[must_use]
    pub fn bit_depth(&self) -> u8 {
        self.bit_depth
    }

    /// The PNG colour type of the stream (IHDR).
    #[must_use]
    pub fn color_type(&self) -> ColorType {
        self.color_type
    }

    /// The exact size in bytes of the **filtered** (inflated) scanline stream: the input a deflater
    /// is handed, and the output an inflater must produce.
    ///
    /// On decode this is also the value the host passes as `max_out`, so an inflater may size its
    /// output buffer from it exactly.
    #[must_use]
    pub fn raw_len(&self) -> usize {
        self.raw_len
    }
}

/// A pluggable **decode-side** backend: inflates the concatenated-IDAT zlib stream.
///
/// The fallback contract: [`supports`](Self::supports) is the only way to decline, and once it
/// accepts, whatever [`inflate`](Self::inflate) returns is the decode's answer. The host re-checks
/// `max_out` after you return, so you can never grow the host's memory past its budget.
pub trait ZlibInflater {
    /// Reports whether this backend wants to inflate the stream described by `info`. Returning
    /// `false` makes the host try the next backend, and finally the built-in `miniz_oxide` tail.
    fn supports(&mut self, info: &IdatInfo) -> bool;

    /// Inflates the zlib stream `zlib`, producing at most `max_out` bytes.
    ///
    /// Stopping at `max_out` is an optimisation, not the security boundary: the host rejects an
    /// over-long result regardless. Every error propagates to the caller: a job is declined only
    /// through [`supports`](Self::supports).
    ///
    /// # Errors
    ///
    /// Whatever the backend reports for a corrupt or truncated stream.
    fn inflate(&mut self, info: &IdatInfo, zlib: &[u8], max_out: usize) -> Result<Vec<u8>>;
}

/// A pluggable **encode-side** backend: produces the zlib stream written as IDAT chunks.
///
/// The returned bytes must be a complete zlib stream (RFC 1950: header, DEFLATE data, Adler-32) of
/// `raw`, which is the already-filtered scanline stream. Any DEFLATE encoding choice is the
/// backend's — PNG only requires that the stream inflate back to `raw`.
pub trait ZlibDeflater {
    /// Reports whether this backend wants to compress the stream described by `info`. Returning
    /// `false` makes the host try the next backend, and finally the built-in [`gamut_deflate`]
    /// tail.
    fn supports(&mut self, info: &IdatInfo) -> bool;

    /// Compresses the filtered scanline stream `raw` into a complete zlib stream.
    ///
    /// Every error propagates to the caller: a job is declined only through
    /// [`supports`](Self::supports).
    ///
    /// # Errors
    ///
    /// Whatever the backend reports on failure.
    fn deflate(&mut self, info: &IdatInfo, raw: &[u8]) -> Result<Vec<u8>>;
}

/// The registry the decoder holds its pushed inflaters in; `Send` is bound here, at insertion.
pub(crate) type Inflaters = Registry<dyn ZlibInflater + Send>;

/// The registry the encoder holds its pushed deflaters in; `Send` is bound here, at insertion.
pub(crate) type Deflaters = Registry<dyn ZlibDeflater + Send>;

/// The error a poisoned backend mutex produces: another thread panicked inside this backend, so its
/// state is not trustworthy.
pub(crate) fn poisoned() -> Error {
    Error::invalid_input(
        env!("CARGO_PKG_NAME"),
        "PNG: IDAT backend mutex was poisoned",
    )
}

/// The host's cap re-check failing — the security invariant of this module.
pub(crate) fn over_cap() -> Error {
    Error::invalid_input(
        env!("CARGO_PKG_NAME"),
        "PNG: IDAT backend produced more than the allowed output size",
    )
}

/// Folds a registry run's failure into this crate's error. A run with a tail never reports
/// [`RegistryError::Unsupported`], so that arm and a backend's own error are the same outcome.
fn registry_error(error: RegistryError<Error>) -> Error {
    match error {
        RegistryError::Poisoned => poisoned(),
        RegistryError::Backend(error) => error,
        RegistryError::Unsupported => Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "PNG: no IDAT backend took the stream",
        ),
    }
}

/// Runs the inflater registry, falling back to `tail` (the built-in `miniz_oxide` inflater).
///
/// This is where the host **re-checks** `max_out` against what the backend actually returned.
pub(crate) fn run_inflaters(
    registry: &Inflaters,
    info: &IdatInfo,
    zlib: &[u8],
    max_out: usize,
    tail: impl FnOnce(&[u8], usize) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    registry
        .run_or(
            |backend| backend.supports(info),
            |backend| {
                let out = backend.inflate(info, zlib, max_out)?;
                // SECURITY: the backend is untrusted with the limit. Reject, never truncate — a
                // truncated stream would be silently wrong image data.
                if out.len() > max_out {
                    return Err(over_cap());
                }
                Ok(out)
            },
            || tail(zlib, max_out),
        )
        .map_err(registry_error)
}

/// Runs the deflater registry, falling back to `tail` (the built-in [`gamut_deflate`] encoder).
pub(crate) fn run_deflaters(
    registry: &Deflaters,
    info: &IdatInfo,
    raw: &[u8],
    tail: impl FnOnce(&[u8]) -> Vec<u8>,
) -> Result<Vec<u8>> {
    registry
        .run_or(
            |backend| backend.supports(info),
            |backend| backend.deflate(info, raw),
            || Ok(tail(raw)),
        )
        .map_err(registry_error)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gamut_core::ErrorKind;

    use super::*;

    /// `gamut_core::Error` is not `Clone` (it wraps `io::Error`), so the test doubles hold a
    /// cloneable description of what to return.
    #[derive(Clone)]
    enum Outcome {
        Produce(Vec<u8>),
        DeclineLate,
        Fail,
    }

    impl Outcome {
        fn to_result(&self) -> Result<Vec<u8>> {
            match self {
                Outcome::Produce(bytes) => Ok(bytes.clone()),
                Outcome::DeclineLate => {
                    Err(Error::unsupported(env!("CARGO_PKG_NAME"), "late decline"))
                }
                Outcome::Fail => Err(Error::invalid_input(env!("CARGO_PKG_NAME"), "boom")),
            }
        }
    }

    struct Fixed {
        accepts: bool,
        outcome: Outcome,
    }

    impl ZlibInflater for Fixed {
        fn supports(&mut self, _info: &IdatInfo) -> bool {
            self.accepts
        }
        fn inflate(&mut self, _info: &IdatInfo, _zlib: &[u8], _max_out: usize) -> Result<Vec<u8>> {
            self.outcome.to_result()
        }
    }

    fn info() -> IdatInfo {
        IdatInfo::new(4, 4, 8, ColorType::Truecolor, 52)
    }

    fn reg(backends: Vec<Fixed>) -> Inflaters {
        let mut r = Registry::new();
        for b in backends {
            r.push(Arc::new(Mutex::new(b)) as Arc<Mutex<dyn ZlibInflater + Send>>);
        }
        r
    }

    #[test]
    fn info_getters_report_every_field() {
        let i = IdatInfo::new(7, 9, 4, ColorType::Indexed, 37);
        assert_eq!(i.width(), 7);
        assert_eq!(i.height(), 9);
        assert_eq!(i.bit_depth(), 4);
        assert_eq!(i.color_type(), ColorType::Indexed);
        assert_eq!(i.raw_len(), 37);
    }

    #[test]
    fn a_tailless_registry_failure_folds_into_an_unsupported_error() {
        let err = registry_error(RegistryError::Unsupported);
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(
            err.static_message(),
            Some("PNG: no IDAT backend took the stream")
        );
    }

    #[test]
    fn over_cap_result_is_rejected_not_truncated() {
        let r = reg(vec![Fixed {
            accepts: true,
            outcome: Outcome::Produce(vec![0u8; 9]),
        }]);
        let err = run_inflaters(&r, &info(), &[], 8, |_, _| {
            unreachable!("tail must not run")
        });
        assert_eq!(err.unwrap_err().to_string(), over_cap().to_string());
        // Exactly at the cap is fine.
        let r = reg(vec![Fixed {
            accepts: true,
            outcome: Outcome::Produce(vec![0u8; 8]),
        }]);
        assert_eq!(
            run_inflaters(&r, &info(), &[], 8, |_, _| unreachable!()).unwrap(),
            vec![0u8; 8]
        );
    }

    #[test]
    fn declines_fall_through_in_push_order_and_reach_the_tail() {
        let r = reg(vec![
            Fixed {
                accepts: false,
                outcome: Outcome::Produce(vec![1]),
            },
            Fixed {
                accepts: true,
                outcome: Outcome::Produce(vec![2]),
            },
            Fixed {
                accepts: true,
                outcome: Outcome::Produce(vec![3]),
            },
        ]);
        assert_eq!(
            run_inflaters(&r, &info(), &[], 8, |_, _| unreachable!()).unwrap(),
            vec![2]
        );
        let all_decline = reg(vec![Fixed {
            accepts: false,
            outcome: Outcome::Produce(vec![1]),
        }]);
        assert_eq!(
            run_inflaters(&all_decline, &info(), &[], 8, |_, _| Ok(vec![9])).unwrap(),
            vec![9]
        );
    }

    #[test]
    fn every_error_after_acceptance_propagates_an_unsupported_one_included() {
        let late = reg(vec![
            Fixed {
                accepts: true,
                outcome: Outcome::DeclineLate,
            },
            Fixed {
                accepts: true,
                outcome: Outcome::Produce(vec![5]),
            },
        ]);
        let err = run_inflaters(&late, &info(), &[], 8, |_, _| unreachable!()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(err.static_message(), Some("late decline"));
        let failing = reg(vec![
            Fixed {
                accepts: true,
                outcome: Outcome::Fail,
            },
            Fixed {
                accepts: true,
                outcome: Outcome::Produce(vec![5]),
            },
        ]);
        let err = run_inflaters(&failing, &info(), &[], 8, |_, _| unreachable!());
        assert_eq!(
            err.unwrap_err().to_string(),
            "invalid input: boom [origin: gamut-png]"
        );
    }

    #[test]
    fn max_out_and_info_reach_the_backend_verbatim() {
        /// One recorded `inflate` call: the cap, the descriptor, and the stream.
        type Call = (usize, IdatInfo, Vec<u8>);
        struct Spy {
            seen: Arc<Mutex<Vec<Call>>>,
        }
        impl ZlibInflater for Spy {
            fn supports(&mut self, _info: &IdatInfo) -> bool {
                true
            }
            fn inflate(&mut self, info: &IdatInfo, zlib: &[u8], max_out: usize) -> Result<Vec<u8>> {
                self.seen
                    .lock()
                    .expect("test mutex")
                    .push((max_out, *info, zlib.to_vec()));
                Ok(Vec::new())
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut r: Inflaters = Registry::new();
        r.push(Arc::new(Mutex::new(Spy { seen: seen.clone() })));
        run_inflaters(&r, &info(), &[7, 7, 7], 1234, |_, _| unreachable!()).unwrap();
        let seen = seen.lock().expect("test mutex");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, 1234);
        assert_eq!(seen[0].1, info());
        assert_eq!(seen[0].2, vec![7, 7, 7]);
    }

    struct FixedDeflater {
        accepts: bool,
        outcome: Outcome,
    }
    impl ZlibDeflater for FixedDeflater {
        fn supports(&mut self, _info: &IdatInfo) -> bool {
            self.accepts
        }
        fn deflate(&mut self, _info: &IdatInfo, _raw: &[u8]) -> Result<Vec<u8>> {
            self.outcome.to_result()
        }
    }

    #[test]
    fn deflater_registry_follows_the_same_contract() {
        let mut r: Deflaters = Registry::new();
        r.push(Arc::new(Mutex::new(FixedDeflater {
            accepts: false,
            outcome: Outcome::Produce(vec![1]),
        })));
        r.push(Arc::new(Mutex::new(FixedDeflater {
            accepts: true,
            outcome: Outcome::Produce(vec![42]),
        })));
        r.push(Arc::new(Mutex::new(FixedDeflater {
            accepts: true,
            outcome: Outcome::Produce(vec![43]),
        })));
        assert_eq!(
            run_deflaters(&r, &info(), &[], |_| unreachable!()).unwrap(),
            vec![42]
        );

        let mut late: Deflaters = Registry::new();
        late.push(Arc::new(Mutex::new(FixedDeflater {
            accepts: true,
            outcome: Outcome::DeclineLate,
        })));
        let err = run_deflaters(&late, &info(), &[], |_| unreachable!()).unwrap_err();
        assert_eq!(err.static_message(), Some("late decline"));

        let mut empty: Deflaters = Registry::new();
        assert_eq!(
            run_deflaters(&empty, &info(), &[1, 2], |raw| raw.to_vec()).unwrap(),
            vec![1, 2]
        );
        empty.push(Arc::new(Mutex::new(FixedDeflater {
            accepts: true,
            outcome: Outcome::Fail,
        })));
        let err = run_deflaters(&empty, &info(), &[], |_| unreachable!());
        assert_eq!(
            err.unwrap_err().to_string(),
            "invalid input: boom [origin: gamut-png]"
        );
    }

    #[test]
    fn a_poisoned_backend_is_an_error_not_a_fall_through() {
        let backend: Arc<Mutex<dyn ZlibInflater + Send>> = Arc::new(Mutex::new(Fixed {
            accepts: true,
            outcome: Outcome::Produce(vec![1]),
        }));
        let poison = backend.clone();
        // Poison the mutex from another thread.
        std::thread::spawn(move || {
            let _guard = poison.lock().expect("first lock");
            panic!("poison");
        })
        .join()
        .expect_err("the thread must panic");
        let mut r: Inflaters = Registry::new();
        r.push(backend);
        let err = run_inflaters(&r, &info(), &[], 8, |_, _| unreachable!());
        assert_eq!(err.unwrap_err().to_string(), poisoned().to_string());
    }
}
