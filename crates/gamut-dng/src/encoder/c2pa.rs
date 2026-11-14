//! The C2PA manifest-store checks: what [`DngEncoder::with_c2pa`] and
//! [`DngEncoder::with_c2pa_reserved`] configured, validated against the container before any pixel
//! work, and the zero-filled reservation materialised without a panic.

use std::borrow::Cow;

use gamut_core::c2pa::MIN_STORE_LEN;
use gamut_core::{Error, Result};
use gamut_ifd::Variant;

use super::DngEncoder;

impl DngEncoder {
    /// The C2PA manifest store to write, if any: the caller's, or a zero-filled reservation.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if both
    /// were requested, if the store is too short to be a
    /// JUMBF box at all ([`MIN_STORE_LEN`]), or if it would pack *inline* in this
    /// encoder's container variant — BigTIFF's inline threshold is those same 8 bytes, so a
    /// BigTIFF store must exceed them to be placeable at the end of the file; if classic TIFF's
    /// 32-bit `LONG` count cannot describe it; or if a reservation is longer than a buffer can
    /// hold ([`zeroed`]). All caught here, before any pixel work.
    pub(super) fn c2pa_store(&self) -> Result<Option<Cow<'_, [u8]>>> {
        // The *length* is settled before a reservation is materialised, so an unusable one costs
        // neither the allocation nor the panic `vec![0; len]` raises past `isize::MAX`.
        let len = match (&self.c2pa, self.c2pa_reserve) {
            (Some(_), Some(_)) => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "DNG: supply either a C2PA manifest store or a reservation, not both",
                ));
            }
            (Some(store), None) => store.len(),
            (None, Some(len)) => len,
            (None, None) => return Ok(None),
        };
        if len < MIN_STORE_LEN {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: a C2PA manifest store is at least a JUMBF box header (8 bytes)",
            ));
        }
        // A value no longer than the variant's inline threshold is packed into the entry rather
        // than placed out of line, so it cannot be the run at the end of the file §A.3.6 wants.
        // Only reachable on BigTIFF, whose threshold is the 8 bytes above.
        if len <= self.variant().inline_threshold() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: a BigTIFF C2PA manifest store must exceed 8 bytes, or it packs inline",
            ));
        }
        // Classic TIFF counts an `UNDEFINED` value with a 32-bit `LONG`; BigTIFF's count is
        // 64-bit. `append_c2pa_store` refuses such a store too, but only once the image has been
        // compressed and the reservation zero-filled. Necessary, not sufficient: the store's
        // offset must also fit, and only `append_c2pa_store` knows the size of the file before it.
        if uncountable_store_len(self.variant(), len) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: a C2PA manifest store longer than 4 GiB cannot be counted by classic \
                 TIFF's 32-bit LONG (BigTIFF's count is 64-bit)",
            ));
        }
        Ok(Some(match &self.c2pa {
            Some(store) => Cow::Borrowed(store.as_slice()),
            None => Cow::Owned(zeroed(len)?),
        }))
    }
}

/// Whether classic TIFF's 32-bit `LONG` count cannot describe a `len`-byte store. BigTIFF's count
/// is 64-bit, so nothing a `usize` holds is uncountable there. A function rather than an inline
/// guard so a mutant inverting it cannot send an oversized length on to [`zeroed`], which would
/// be a multi-GiB zero-fill and a timed-out mutant rather than a caught one.
fn uncountable_store_len(variant: Variant, len: usize) -> bool {
    match variant {
        Variant::Classic => u32::try_from(len).is_err(),
        // BigTIFF's 64-bit count (and any wider variant `gamut_ifd` adds) names every `usize`.
        _ => false,
    }
}

/// A `len`-byte zero-filled C2PA reservation, or a typed error where `vec![0; len]` would panic.
///
/// [`DngEncoder::with_c2pa_reserved`] returns `Self`, so it cannot refuse anything itself and the
/// length it stores is a caller's number that reaches this untouched. Past `isize::MAX` a
/// `Vec<u8>` cannot exist, and `vec![0; len]` panics with a capacity overflow — which a library
/// path must not do. Reserving fallibly turns that into an
/// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error. A reservation the
/// machine merely lacks memory for still aborts, as any oversized allocation in Rust does.
fn zeroed(len: usize) -> Result<Vec<u8>> {
    let mut store = Vec::new();
    store.try_reserve_exact(len).map_err(|_| {
        Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: a C2PA manifest store reservation this long cannot be allocated",
        )
    })?;
    store.resize(len, 0);
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A C2PA reservation's length is a caller's number that `with_c2pa_reserved` cannot refuse,
    /// and it went straight into `vec![0; len]`, which panics with a capacity overflow past
    /// `isize::MAX` (#552). `usize::MAX` reaches that on BigTIFF, whose 64-bit count admits it;
    /// classic TIFF refuses it one check earlier, for its 32-bit count.
    #[test]
    fn a_reservation_no_buffer_could_hold_is_refused_instead_of_panicking() {
        let err = DngEncoder::new()
            .with_variant(Variant::Big)
            .with_c2pa_reserved(usize::MAX)
            .c2pa_store()
            .expect_err("no buffer holds usize::MAX bytes");
        assert!(err.to_string().contains("cannot be allocated"), "{err}");

        let err = DngEncoder::new()
            .with_c2pa_reserved(usize::MAX)
            .c2pa_store()
            .expect_err("classic TIFF cannot count usize::MAX bytes");
        assert!(err.to_string().contains("32-bit LONG"), "{err}");
    }

    /// Classic TIFF's count word holds `u32::MAX` and nothing past it; BigTIFF's holds any
    /// `usize`. Asserted on the predicate, since the accepting side through `c2pa_store` would
    /// mean zero-filling 4 GiB. 64-bit only: a 32-bit `usize` has no length past `u32::MAX`.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn only_classic_tiff_cannot_count_a_store_past_u32_max() {
        let max = u32::MAX as usize;
        assert!(!uncountable_store_len(Variant::Classic, max));
        assert!(uncountable_store_len(Variant::Classic, max + 1));
        assert!(!uncountable_store_len(Variant::Big, usize::MAX));
    }
}
