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
//! Out of the box the choice is compile-time: encode goes through the in-house, encoder-only
//! [`gamut_deflate`] (zopfli-class), decode inflates via `miniz_oxide`. Both stay as the implicit
//! *tail* of the registry; nothing changes for a caller that pushes no backend.
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
//! [`IdatInflater`] / [`IdatDeflater`] are **PNG's own traits**, not a shared `gamut-deflate`
//! abstraction. `gamut-deflate` is unchanged by this module and gains no plugin surface, and the
//! TIFF/DNG Deflate code paths are deliberately **not** routed through this seam. The datum here
//! carries PNG context ([`IdatInfo`]) that a general-purpose DEFLATE abstraction has no business
//! knowing, and PNG's security posture (see below) is PNG's to enforce.
//!
//! # The fallback contract (#241)
//!
//! Identical in both directions and to every other gamut format crate:
//!
//! 1. Backends are tried in **push order**.
//! 2. [`supports`](IdatInflater::supports) returning `false` is the *only* signal that falls
//!    through to the next backend. A backend that declines late does so by returning
//!    [`Error::Unsupported`] — the C-ABI adapters map
//!    [`Status::UNSUPPORTED`](gamut_codec_abi::Status::UNSUPPORTED) onto exactly that.
//! 3. A backend that **accepts** and then **fails** propagates its error to the caller. The host
//!    does not retry a later backend or the built-in tail, because a partially-produced result must
//!    never be silently masked.
//! 4. The built-in implementation is the implicit **tail**, tried last: [`gamut_deflate`] for
//!    encode, `miniz_oxide` for decode. Should the in-house inflater of issue #196 land, it slots
//!    into the decode tail with no change to this surface.
//!
//! # Security invariant: the host owns the zlib-bomb cap
//!
//! PNG input is attacker-controlled and a tiny IDAT can claim to inflate without bound. The output
//! cap is therefore enforced **by the host, not by the backend**:
//!
//! - `max_out` is passed to [`IdatInflater::inflate`] so a cooperative backend can stop early, and
//! - the host **re-checks the returned length** afterwards. A backend that returns more than
//!   `max_out` bytes is rejected with [`Error::InvalidInput`] — never truncated, never trusted.
//!
//! A third-party or FFI backend is untrusted code; the limit survives a backend that ignores or
//! misimplements it.
//!
//! # Example
//!
//! ```
//! use gamut_core::Result;
//! use gamut_png::{IdatDeflater, IdatInfo, PngEncoder};
//!
//! /// A deflater that only handles 8-bit images and stores them uncompressed.
//! struct Stored;
//! impl IdatDeflater for Stored {
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
//! encoder.push_backend(Stored);
//! ```

use std::sync::{Arc, Mutex};

use gamut_core::{Error, ErrorKind, Result};

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
    /// to unit-test an [`IdatInflater`] / [`IdatDeflater`] implementation directly.
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
/// See the [module docs](self) for the fallback contract and the security invariant. In short:
/// [`supports`](Self::supports) is how you decline; the host re-checks `max_out` after you return,
/// so you can never grow the host's memory past its budget.
pub trait IdatInflater: Send {
    /// Reports whether this backend wants to inflate the stream described by `info`. Returning
    /// `false` makes the host try the next backend, and finally the built-in `miniz_oxide` tail.
    fn supports(&mut self, info: &IdatInfo) -> bool;

    /// Inflates the zlib stream `zlib`, producing at most `max_out` bytes.
    ///
    /// Stopping at `max_out` is an optimisation, not the security boundary: the host rejects an
    /// over-long result regardless. Return [`Error::Unsupported`] to decline the job late (the host
    /// then falls through); any other error propagates to the caller.
    ///
    /// # Errors
    ///
    /// Whatever the backend reports for a corrupt or truncated stream, or
    /// [`Error::Unsupported`] to decline.
    fn inflate(&mut self, info: &IdatInfo, zlib: &[u8], max_out: usize) -> Result<Vec<u8>>;
}

/// A pluggable **encode-side** backend: produces the zlib stream written as IDAT chunks.
///
/// The returned bytes must be a complete zlib stream (RFC 1950: header, DEFLATE data, Adler-32) of
/// `raw`, which is the already-filtered scanline stream. Any DEFLATE encoding choice is the
/// backend's — PNG only requires that the stream inflate back to `raw`.
pub trait IdatDeflater: Send {
    /// Reports whether this backend wants to compress the stream described by `info`. Returning
    /// `false` makes the host try the next backend, and finally the built-in [`gamut_deflate`]
    /// tail.
    fn supports(&mut self, info: &IdatInfo) -> bool;

    /// Compresses the filtered scanline stream `raw` into a complete zlib stream.
    ///
    /// Return [`Error::Unsupported`] to decline the job late (the host then falls through); any
    /// other error propagates to the caller.
    ///
    /// # Errors
    ///
    /// Whatever the backend reports on failure, or [`Error::Unsupported`] to decline.
    fn deflate(&mut self, info: &IdatInfo, raw: &[u8]) -> Result<Vec<u8>>;
}

/// The host-owned registry: backends in push order, behind `Arc<Mutex<…>>`.
///
/// `Arc` because [`crate::PngEncoder`] / [`crate::PngDecoder`] are `Clone` and their encode/decode
/// methods take `&self`; `Mutex` because the trait methods take `&mut self` (a backend commonly
/// owns a device handle or scratch arena).
pub(crate) struct Registry<T: ?Sized>(Vec<Arc<Mutex<T>>>);

impl<T: ?Sized> Registry<T> {
    /// Appends a backend to the end of the try order.
    pub(crate) fn push(&mut self, backend: Arc<Mutex<T>>) {
        self.0.push(backend);
    }

    /// The backends, in push order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<Mutex<T>>> {
        self.0.iter()
    }

    /// The number of registered backends.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

// Manual impls: `dyn Trait` is neither `Debug` nor `Clone`, but the outer builders must stay both.
impl<T: ?Sized> core::fmt::Debug for Registry<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Registry({} backend(s))", self.0.len())
    }
}

impl<T: ?Sized> Clone for Registry<T> {
    /// Clones the *handles*: a cloned encoder or decoder **shares** the same backend instances.
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: ?Sized> Default for Registry<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

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

/// Runs the inflater registry, falling back to `tail` (the built-in `miniz_oxide` inflater).
///
/// This is where the host **re-checks** `max_out` against what the backend actually returned.
pub(crate) fn run_inflaters(
    registry: &Registry<dyn IdatInflater + Send>,
    info: &IdatInfo,
    zlib: &[u8],
    max_out: usize,
    tail: impl FnOnce(&[u8], usize) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    for backend in registry.iter() {
        let mut backend = backend.lock().map_err(|_| poisoned())?;
        if !backend.supports(info) {
            continue;
        }
        let out = match backend.inflate(info, zlib, max_out) {
            // A late decline (the C-ABI `Status::UNSUPPORTED`) falls through, like `supports`.
            Err(error) if error.kind() == ErrorKind::Unsupported => continue,
            other => other?,
        };
        // SECURITY: the backend is untrusted with the limit. Reject, never truncate — a truncated
        // stream would be silently wrong image data.
        if out.len() > max_out {
            return Err(over_cap());
        }
        return Ok(out);
    }
    tail(zlib, max_out)
}

/// Runs the deflater registry, falling back to `tail` (the built-in [`gamut_deflate`] encoder).
pub(crate) fn run_deflaters(
    registry: &Registry<dyn IdatDeflater + Send>,
    info: &IdatInfo,
    raw: &[u8],
    tail: impl FnOnce(&[u8]) -> Vec<u8>,
) -> Result<Vec<u8>> {
    for backend in registry.iter() {
        let mut backend = backend.lock().map_err(|_| poisoned())?;
        if !backend.supports(info) {
            continue;
        }
        match backend.deflate(info, raw) {
            Err(error) if error.kind() == ErrorKind::Unsupported => continue,
            other => return other,
        }
    }
    Ok(tail(raw))
}

#[cfg(test)]
mod tests {
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
                Outcome::DeclineLate => Err(Error::Unsupported("late decline")),
                Outcome::Fail => Err(Error::InvalidInput("boom")),
            }
        }
    }

    struct Fixed {
        accepts: bool,
        outcome: Outcome,
    }

    impl IdatInflater for Fixed {
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

    fn reg(backends: Vec<Fixed>) -> Registry<dyn IdatInflater + Send> {
        let mut r = Registry::default();
        for b in backends {
            r.push(Arc::new(Mutex::new(b)) as Arc<Mutex<dyn IdatInflater + Send>>);
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
    fn registry_debug_and_clone_report_and_share() {
        let r = reg(vec![Fixed {
            accepts: true,
            outcome: Outcome::Produce(vec![1]),
        }]);
        assert_eq!(format!("{r:?}"), "Registry(1 backend(s))");
        let c = r.clone();
        assert_eq!(c.len(), 1);
        assert!(Arc::ptr_eq(
            r.iter().next().unwrap(),
            c.iter().next().unwrap()
        ));
        assert_eq!(Registry::<dyn IdatInflater + Send>::default().len(), 0);
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
    fn late_unsupported_declines_but_other_errors_propagate() {
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
        assert_eq!(
            run_inflaters(&late, &info(), &[], 8, |_, _| unreachable!()).unwrap(),
            vec![5]
        );
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
        assert_eq!(err.unwrap_err().to_string(), "invalid input: boom");
    }

    #[test]
    fn max_out_and_info_reach_the_backend_verbatim() {
        /// One recorded `inflate` call: the cap, the descriptor, and the stream.
        type Call = (usize, IdatInfo, Vec<u8>);
        struct Spy {
            seen: Arc<Mutex<Vec<Call>>>,
        }
        impl IdatInflater for Spy {
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
        let mut r: Registry<dyn IdatInflater + Send> = Registry::default();
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
    impl IdatDeflater for FixedDeflater {
        fn supports(&mut self, _info: &IdatInfo) -> bool {
            self.accepts
        }
        fn deflate(&mut self, _info: &IdatInfo, _raw: &[u8]) -> Result<Vec<u8>> {
            self.outcome.to_result()
        }
    }

    #[test]
    fn deflater_registry_follows_the_same_contract() {
        let mut r: Registry<dyn IdatDeflater + Send> = Registry::default();
        r.push(Arc::new(Mutex::new(FixedDeflater {
            accepts: false,
            outcome: Outcome::Produce(vec![1]),
        })));
        r.push(Arc::new(Mutex::new(FixedDeflater {
            accepts: true,
            outcome: Outcome::DeclineLate,
        })));
        r.push(Arc::new(Mutex::new(FixedDeflater {
            accepts: true,
            outcome: Outcome::Produce(vec![42]),
        })));
        assert_eq!(
            run_deflaters(&r, &info(), &[], |_| unreachable!()).unwrap(),
            vec![42]
        );

        let mut empty: Registry<dyn IdatDeflater + Send> = Registry::default();
        assert_eq!(
            run_deflaters(&empty, &info(), &[1, 2], |raw| raw.to_vec()).unwrap(),
            vec![1, 2]
        );
        empty.push(Arc::new(Mutex::new(FixedDeflater {
            accepts: true,
            outcome: Outcome::Fail,
        })));
        let err = run_deflaters(&empty, &info(), &[], |_| unreachable!());
        assert_eq!(err.unwrap_err().to_string(), "invalid input: boom");
    }

    #[test]
    fn a_poisoned_backend_is_an_error_not_a_fall_through() {
        let backend: Arc<Mutex<dyn IdatInflater + Send>> = Arc::new(Mutex::new(Fixed {
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
        let mut r: Registry<dyn IdatInflater + Send> = Registry::default();
        r.push(backend);
        let err = run_inflaters(&r, &info(), &[], 8, |_, _| unreachable!());
        assert_eq!(err.unwrap_err().to_string(), poisoned().to_string());
    }
}
