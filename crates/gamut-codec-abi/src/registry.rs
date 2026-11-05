//! The one backend registry every format crate holds its pushed backends in (`std` feature).
//!
//! It implements the crate-level fallback contract once, generically over the format's own typed
//! backend trait `T`, so no format crate re-derives it:
//!
//! - backends are tried in **push order**;
//! - `supports == false` is the **only** fall-through;
//! - a backend that accepts and then fails returns its error **unchanged**
//!   ([`RegistryError::Backend`]) and no later backend is tried;
//! - a lock a panicking backend poisoned is an error ([`RegistryError::Poisoned`]), never silently
//!   recovered;
//! - with no backend accepting and no tail, the job is [`RegistryError::Unsupported`].
//!
//! `Send` is not demanded by the registry: a host that needs it names `T` as `dyn Trait + Send`
//! where it inserts the backend.

use alloc::vec::Vec;
use core::fmt;
use std::sync::{Arc, Mutex};

/// Why a [`Registry`] run produced no result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError<E> {
    /// No backend accepted the job and there was no tail to run instead.
    Unsupported,
    /// A backend's lock was poisoned by a panic while it was held.
    Poisoned,
    /// The backend (or tail) that accepted the job failed; its error, unchanged.
    Backend(E),
}

/// An ordered list of pushed backends, tried in push order.
///
/// Each slot is an `Arc<Mutex<T>>`: the `Mutex` lets a host's `&self` methods call a backend's
/// `&mut self` ones, and the `Arc` lets one backend instance sit in several registries. Cloning a
/// registry clones the **list**: the clone and the original share the backend instances, but a
/// later [`push`](Self::push) to one is not seen by the other.
pub struct Registry<T: ?Sized> {
    backends: Vec<Arc<Mutex<T>>>,
}

impl<T: ?Sized> Registry<T> {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    /// Appends `backend` to the end of the try order (still ahead of any tail).
    pub fn push(&mut self, backend: Arc<Mutex<T>>) {
        self.backends.push(backend);
    }

    /// The number of pushed backends.
    #[must_use]
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// Whether no backend has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// Runs the job on the first backend whose `supports` accepts it.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Backend`] with the accepting backend's error, unchanged;
    /// [`RegistryError::Poisoned`] when a backend's lock is poisoned; and
    /// [`RegistryError::Unsupported`] when no backend accepts.
    pub fn run<R, E>(
        &self,
        supports: impl FnMut(&mut T) -> bool,
        call: impl FnMut(&mut T) -> Result<R, E>,
    ) -> Result<R, RegistryError<E>> {
        self.first_accepting(supports, call)?
            .ok_or(RegistryError::Unsupported)
    }

    /// Runs the job on the first backend whose `supports` accepts it, or on `tail` (the crate's
    /// built-in implementation) when none does.
    ///
    /// # Errors
    ///
    /// As [`run`](Self::run), except that a declined job runs `tail`, whose error is
    /// [`RegistryError::Backend`]; this never returns [`RegistryError::Unsupported`].
    pub fn run_or<R, E>(
        &self,
        supports: impl FnMut(&mut T) -> bool,
        call: impl FnMut(&mut T) -> Result<R, E>,
        tail: impl FnOnce() -> Result<R, E>,
    ) -> Result<R, RegistryError<E>> {
        match self.first_accepting(supports, call)? {
            Some(result) => Ok(result),
            None => tail().map_err(RegistryError::Backend),
        }
    }

    /// The push-order walk both runs share: `Ok(None)` when every backend declined.
    fn first_accepting<R, E>(
        &self,
        mut supports: impl FnMut(&mut T) -> bool,
        mut call: impl FnMut(&mut T) -> Result<R, E>,
    ) -> Result<Option<R>, RegistryError<E>> {
        for slot in &self.backends {
            let mut backend = slot.lock().map_err(|_| RegistryError::Poisoned)?;
            if supports(&mut *backend) {
                return call(&mut *backend)
                    .map(Some)
                    .map_err(RegistryError::Backend);
            }
        }
        Ok(None)
    }
}

// Manual impls: `dyn Trait` is neither `Clone` nor `Debug`, and neither bound is needed on `T`.
impl<T: ?Sized> Clone for Registry<T> {
    fn clone(&self) -> Self {
        Self {
            backends: self.backends.clone(),
        }
    }
}

impl<T: ?Sized> Default for Registry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ?Sized> fmt::Debug for Registry<T> {
    /// Backends are opaque, so only their count is shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("backends", &self.backends.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::{format, vec};

    use super::*;

    /// A test double: accepts when `accepts`, then answers `result`, logging every call by `id`.
    struct Fake {
        id: u8,
        accepts: bool,
        result: Result<u8, u8>,
    }

    impl Fake {
        fn slot(id: u8, accepts: bool, result: Result<u8, u8>) -> Arc<Mutex<Fake>> {
            Arc::new(Mutex::new(Fake {
                id,
                accepts,
                result,
            }))
        }
    }

    /// Runs `registry` and returns its answer plus the ids `supports` and `call` saw, in order.
    fn run_logged(
        registry: &Registry<Fake>,
        tail: Option<Result<u8, u8>>,
    ) -> (Result<u8, RegistryError<u8>>, Vec<u8>, Vec<u8>) {
        let mut asked = Vec::new();
        let mut called = Vec::new();
        let supports = |b: &mut Fake| {
            asked.push(b.id);
            b.accepts
        };
        let call = |b: &mut Fake| {
            called.push(b.id);
            b.result
        };
        let out = match tail {
            Some(t) => registry.run_or(supports, call, || t),
            None => registry.run(supports, call),
        };
        (out, asked, called)
    }

    fn registry(slots: &[Arc<Mutex<Fake>>]) -> Registry<Fake> {
        let mut registry = Registry::new();
        for slot in slots {
            registry.push(Arc::clone(slot));
        }
        registry
    }

    #[test]
    fn run_tries_backends_in_push_order_and_stops_at_the_first_acceptor() {
        let reg = registry(&[
            Fake::slot(1, false, Ok(10)),
            Fake::slot(2, true, Ok(20)),
            Fake::slot(3, true, Ok(30)),
        ]);
        let (out, asked, called) = run_logged(&reg, None);
        assert_eq!(out, Ok(20));
        assert_eq!(asked, vec![1, 2]);
        assert_eq!(called, vec![2]);
    }

    #[test]
    fn run_propagates_an_accepting_backends_error_without_trying_the_next() {
        let reg = registry(&[Fake::slot(1, true, Err(7)), Fake::slot(2, true, Ok(20))]);
        let (out, asked, called) = run_logged(&reg, Some(Ok(99)));
        assert_eq!(out, Err(RegistryError::Backend(7)));
        assert_eq!(asked, vec![1]);
        assert_eq!(called, vec![1]);
    }

    #[test]
    fn run_is_unsupported_when_every_backend_declines() {
        let reg = registry(&[Fake::slot(1, false, Ok(10)), Fake::slot(2, false, Ok(20))]);
        let (out, asked, called) = run_logged(&reg, None);
        assert_eq!(out, Err(RegistryError::Unsupported));
        assert_eq!(asked, vec![1, 2]);
        assert!(called.is_empty());
    }

    #[test]
    fn run_on_an_empty_registry_is_unsupported() {
        let (out, asked, _) = run_logged(&Registry::new(), None);
        assert_eq!(out, Err(RegistryError::Unsupported));
        assert!(asked.is_empty());
    }

    #[test]
    fn run_or_runs_the_tail_only_when_every_backend_declines() {
        let declining = registry(&[Fake::slot(1, false, Ok(10))]);
        assert_eq!(run_logged(&declining, Some(Ok(99))).0, Ok(99));
        assert_eq!(
            run_logged(&declining, Some(Err(5))).0,
            Err(RegistryError::Backend(5))
        );
        assert_eq!(run_logged(&Registry::new(), Some(Ok(99))).0, Ok(99));

        let accepting = registry(&[Fake::slot(1, true, Ok(10))]);
        assert_eq!(run_logged(&accepting, Some(Ok(99))).0, Ok(10));
    }

    #[test]
    fn a_poisoned_backend_lock_is_an_error() {
        let poisoned = Fake::slot(1, true, Ok(10));
        let holder = Arc::clone(&poisoned);
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _guard = holder.lock();
            panic!("backend panicked while locked");
        }));
        assert!(panicked.is_err());

        let reg = registry(&[poisoned, Fake::slot(2, true, Ok(20))]);
        let (out, asked, _) = run_logged(&reg, Some(Ok(99)));
        assert_eq!(out, Err(RegistryError::Poisoned));
        assert!(asked.is_empty());
    }

    #[test]
    fn a_clone_has_an_independent_list_over_shared_backends() {
        let shared = Fake::slot(1, false, Ok(10));
        let mut original = registry(&[Arc::clone(&shared)]);
        let clone = original.clone();
        original.push(Fake::slot(2, true, Ok(20)));
        assert_eq!((original.len(), clone.len()), (2, 1));

        // The instance itself is shared: a change through the original is seen by the clone.
        shared.lock().map(|mut b| b.accepts = true).ok();
        assert_eq!(run_logged(&clone, None).0, Ok(10));
    }

    #[test]
    fn len_and_is_empty_count_pushed_backends() {
        let mut reg = Registry::<Fake>::default();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        reg.push(Fake::slot(1, true, Ok(10)));
        assert!(!reg.is_empty());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn debug_shows_only_the_backend_count() {
        let reg = registry(&[Fake::slot(1, true, Ok(10)), Fake::slot(2, true, Ok(20))]);
        assert_eq!(format!("{reg:?}"), "Registry { backends: 2 }");
    }
}
