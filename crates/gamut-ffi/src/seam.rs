//! The one shape of a backend-seam handle.
//!
//! `seam_handle!` stamps the `_new` / `_free` / `_push_backend` internals for one format host,
//! so validation order, status mapping, ownership transfer, and panic containment exist in
//! exactly one place. The `#[unsafe(no_mangle)] extern "C"` wrappers stay handwritten in the
//! format modules — one line each, visible to cbindgen's plain (non-expanding) parse and
//! carrying the C-facing documentation — and the macro locks them: a `const` pin per wrapper
//! makes a missing wrapper an unresolved name and a drifted signature a type mismatch, both
//! compile errors. The macro also stamps the per-handle boundary contract test, so every
//! handle proves the same lifecycle without hand-copied test code.
//!
//! Convention: the handle struct is handwritten (cbindgen needs to see it for the opaque
//! forward declaration) and holds its host in a field named `inner`. A handle that carries
//! per-handle state beside its host names each extra field's initial value in an optional
//! `fields = { name: value, … }` entry, which `_new` fills in alongside `inner`.
//!
//! The macro also implements [`Handle`] for the struct, which is what the one configure path —
//! [`configure`] and [`try_configure`], behind every `gamut_<fmt>_<dir>_set_*` setter — reaches
//! the host through.

use crate::guard;
use crate::status::{GAMUT_OK, GAMUT_STATUS_NULL_ARGUMENT, GamutStatus};

/// A seam handle: the opaque struct `seam_handle!` stamps, holding its format host in `inner`.
#[cfg_attr(
    not(feature = "avif"),
    expect(
        dead_code,
        reason = "only the AVIF encoder has config setters yet (issue #242)"
    )
)]
pub(crate) trait Handle {
    /// The format crate's host (`AvifEncoder`, `PngDecoder`, …) the handle wraps.
    type Host;

    /// The handle's host.
    fn host_mut(&mut self) -> &mut Self::Host;
}

/// Applies one consuming `with_*` builder to the host behind `handle`.
///
/// `value` is the C argument already mapped onto its Rust type; `None` means the mapping
/// rejected it, reported as `rejected` with the host unchanged. A null handle is reported first,
/// whatever `value`.
///
/// # Safety
///
/// `handle` is `NULL` or a live pointer from the handle's constructor that has not been freed.
#[cfg_attr(
    not(feature = "avif"),
    expect(
        dead_code,
        reason = "only the AVIF encoder has config setters yet (issue #242)"
    )
)]
pub(crate) unsafe fn configure<H: Handle, T>(
    handle: *mut H,
    value: Option<T>,
    rejected: GamutStatus,
    apply: impl FnOnce(H::Host, T) -> H::Host,
) -> GamutStatus
where
    H::Host: Default,
{
    guard::status(|| {
        // SAFETY: null is rejected here; a non-null handle is live per contract.
        let Some(this) = (unsafe { handle.as_mut() }) else {
            return GAMUT_STATUS_NULL_ARGUMENT;
        };
        let Some(value) = value else {
            return rejected;
        };
        // The Rust builders consume and return the host (backends included), so it is moved out
        // and back; the `Default` left behind for that instant is never observed, because no
        // `with_*` builder can panic.
        let host = this.host_mut();
        *host = apply(core::mem::take(host), value);
        GAMUT_OK
    })
}

/// [`configure`] for a fallible builder: on its refusal the handle keeps the host it had and the
/// refusal's status is returned.
///
/// # Safety
///
/// As [`configure`].
#[cfg_attr(
    not(feature = "avif"),
    expect(
        dead_code,
        reason = "only the AVIF encoder has config setters yet (issue #242)"
    )
)]
pub(crate) unsafe fn try_configure<H: Handle, T>(
    handle: *mut H,
    value: T,
    apply: impl FnOnce(H::Host, T) -> gamut::core::Result<H::Host>,
) -> GamutStatus
where
    H::Host: Clone,
{
    guard::status(|| {
        // SAFETY: null is rejected here; a non-null handle is live per contract.
        let Some(this) = (unsafe { handle.as_mut() }) else {
            return GAMUT_STATUS_NULL_ARGUMENT;
        };
        let host = this.host_mut();
        // A clone shares the backends, so the handle's own host is untouched unless the builder
        // accepts.
        match apply(host.clone(), value) {
            Ok(configured) => {
                *host = configured;
                GAMUT_OK
            }
            Err(error) => crate::boundary::status_of(&error),
        }
    })
}

macro_rules! seam_handle {
    (
        handle = $Handle:ident,
        host = $Host:ty,
        new = $new:expr,
        $(fields = { $($field:ident : $value:expr),* $(,)? },)?
        seam = decoder,
        push = |$host:ident, $backend:ident| $push:expr,
        fns = ($new_fn:ident, $free_fn:ident, $push_fn:ident),
        tests = $tests:ident $(,)?
    ) => {
        seam_handle!(@impl
            $Handle, $Host, $new, { $($($field : $value),*)? },
            ::gamut_codec_abi::DecoderVTable,
            ::gamut_codec_abi::bridge::ForeignDecoder,
            crate::test_support::decoder_vtable,
            |$host, $backend| $push,
            ($new_fn, $free_fn, $push_fn),
            $tests
        );
    };
    (
        handle = $Handle:ident,
        host = $Host:ty,
        new = $new:expr,
        $(fields = { $($field:ident : $value:expr),* $(,)? },)?
        seam = encoder,
        push = |$host:ident, $backend:ident| $push:expr,
        fns = ($new_fn:ident, $free_fn:ident, $push_fn:ident),
        tests = $tests:ident $(,)?
    ) => {
        seam_handle!(@impl
            $Handle, $Host, $new, { $($($field : $value),*)? },
            ::gamut_codec_abi::EncoderVTable,
            ::gamut_codec_abi::bridge::ForeignEncoder,
            crate::test_support::encoder_vtable,
            |$host, $backend| $push,
            ($new_fn, $free_fn, $push_fn),
            $tests
        );
    };
    (@impl
        $Handle:ident, $Host:ty, $new:expr, { $($field:ident : $value:expr),* },
        $VTable:ty, $Foreign:ty, $vt_builder:path,
        |$host:ident, $backend:ident| $push:expr,
        ($new_fn:ident, $free_fn:ident, $push_fn:ident),
        $tests:ident
    ) => {
        impl crate::seam::Handle for $Handle {
            type Host = $Host;

            fn host_mut(&mut self) -> &mut $Host {
                &mut self.inner
            }
        }

        impl $Handle {
            fn ffi_new() -> *mut Self {
                crate::guard::ptr(|| {
                    Box::into_raw(Box::new(Self { inner: $new, $($field: $value),* }))
                })
            }

            /// # Safety
            ///
            /// `handle` is `NULL` or a live pointer from `ffi_new` that has not been freed.
            unsafe fn ffi_free(handle: *mut Self) {
                crate::guard::unit(|| {
                    if !handle.is_null() {
                        // SAFETY: per this fn's contract; dropping the host drops every
                        // adopted backend, running each vtable's `destroy` exactly once.
                        drop(unsafe { Box::from_raw(handle) });
                    }
                });
            }

            /// # Safety
            ///
            /// `handle` as in `ffi_free`; `vtable`, when non-null, points to a vtable that
            /// stays valid for the handle's lifetime; on `GAMUT_OK` the handle owns `ctx`.
            unsafe fn ffi_push_backend(
                handle: *mut Self,
                vtable: *const $VTable,
                ctx: *mut ::core::ffi::c_void,
            ) -> crate::status::GamutStatus {
                crate::guard::status(|| {
                    // SAFETY: null is rejected here; a non-null handle is live per contract.
                    let Some(this) = (unsafe { handle.as_mut() }) else {
                        return crate::status::GAMUT_STATUS_NULL_ARGUMENT;
                    };
                    if vtable.is_null() {
                        return crate::status::GAMUT_STATUS_NULL_ARGUMENT;
                    }
                    // SAFETY: non-null and caller-guaranteed valid; the bridge constructor
                    // re-checks the ABI generation.
                    let Some($backend) = (unsafe { <$Foreign>::new(vtable, ctx) }) else {
                        // The caller keeps ownership of `ctx`; no `destroy` has run.
                        return crate::status::GAMUT_STATUS_ABI_MISMATCH;
                    };
                    let $host = &mut this.inner;
                    $push;
                    crate::status::GAMUT_OK
                })
            }
        }

        // Wrapper completeness and signature locks: a missing `#[unsafe(no_mangle)]` wrapper
        // is an unresolved name, a drifted wrapper signature a type mismatch — both compile
        // errors, in both directions between this table entry and the handwritten fns.
        const _: extern "C" fn() -> *mut $Handle = $new_fn;
        const _: unsafe extern "C" fn(*mut $Handle) = $free_fn;
        const _: unsafe extern "C" fn(
            *mut $Handle,
            *const $VTable,
            *mut ::core::ffi::c_void,
        ) -> crate::status::GamutStatus = $push_fn;

        #[cfg(test)]
        mod $tests {
            use super::*;

            /// The uniform boundary contract, stamped identically for every handle.
            #[test]
            fn lifecycle_and_push_contract() {
                use ::std::sync::atomic::{AtomicUsize, Ordering};

                use crate::status::{
                    GAMUT_OK, GAMUT_STATUS_ABI_MISMATCH, GAMUT_STATUS_NULL_ARGUMENT,
                };

                let destroyed = AtomicUsize::new(0);
                let ctx = ::std::ptr::from_ref(&destroyed).cast_mut().cast();
                let vt = $vt_builder();

                let handle = $new_fn();
                assert!(!handle.is_null());

                // Null arguments are rejected before anything is adopted.
                assert_eq!(
                    unsafe { $push_fn(::std::ptr::null_mut(), &vt, ctx) },
                    GAMUT_STATUS_NULL_ARGUMENT
                );
                assert_eq!(
                    unsafe { $push_fn(handle, ::std::ptr::null(), ctx) },
                    GAMUT_STATUS_NULL_ARGUMENT
                );

                // A vtable from another library generation is rejected; the caller keeps
                // `ctx` and no `destroy` runs.
                let mut stale = vt;
                stale.abi_version = stale.abi_version.wrapping_add(1);
                assert_eq!(
                    unsafe { $push_fn(handle, &stale, ctx) },
                    GAMUT_STATUS_ABI_MISMATCH
                );
                assert_eq!(destroyed.load(Ordering::SeqCst), 0);

                // An accepted push transfers `ctx`; `_free` tears it down exactly once.
                assert_eq!(unsafe { $push_fn(handle, &vt, ctx) }, GAMUT_OK);
                assert_eq!(destroyed.load(Ordering::SeqCst), 0);
                unsafe { $free_fn(handle) };
                assert_eq!(destroyed.load(Ordering::SeqCst), 1);

                // `_free` is null-tolerant.
                unsafe { $free_fn(::std::ptr::null_mut()) };
            }
        }
    };
}

pub(crate) use seam_handle;
