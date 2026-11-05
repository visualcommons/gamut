//! Locating a directory's pixel data: the strip height and the checked byte extents its
//! offset/byte-count tag pairs name.
//!
//! TIFF 6.0 §3 stores image data as strips (`StripOffsets` 273 / `StripByteCounts` 279, banded
//! by `RowsPerStrip` 278) and §15 as tiles (`TileOffsets` 324 / `TileByteCounts` 325). Every
//! TIFF-family decoder resolves those pairs into byte slices before it decompresses anything, so
//! the arithmetic lives here once, in 64 bits and checked: a BigTIFF writes the pairs as `LONG8`,
//! and a classic file's `offset + count` can pass `u32::MAX` — on a 32-bit target, `usize::MAX`
//! too. What stays with the codec is *policy*: whether a `RowsPerStrip` of zero is a file to read
//! or one to refuse ([`ZeroRowsPerStrip`]).

use gamut_core::{Error, Result};

/// What a reader makes of a `RowsPerStrip` (278) of zero.
///
/// TIFF 6.0 §8 gives the tag no zero: its default, 2**32 − 1, already means "one strip", and a
/// zero would divide the height by nothing. Readers disagree on the file that carries one anyway,
/// so the choice is the caller's.
///
/// Non-exhaustive, with an explicit `repr` and permanent discriminants (the workspace's C-ABI
/// convention for fieldless enums).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u8)]
pub enum ZeroRowsPerStrip {
    /// Read it as the absent tag: the whole image is one strip (gamut-tiff, like libtiff).
    WholeImage = 0,
    /// Refuse it as [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput)
    /// (gamut-dng, whose specification leaves no reading of it).
    Refuse = 1,
}

/// The number of image rows in each strip of an image `height` rows tall, from the directory's
/// `RowsPerStrip` value `tag` (`None` when the tag is absent).
///
/// An absent tag is the TIFF 6.0 §8 default — one strip of the whole height — and a zero is read
/// or refused as `zero` says. Any other value is returned as stored, even past `height`: the last
/// strip is the short one, so a value past the height already describes one strip.
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error reported by `origin`
/// when `tag` is zero and `zero` is [`ZeroRowsPerStrip::Refuse`].
pub fn rows_per_strip(
    tag: Option<u32>,
    height: usize,
    zero: ZeroRowsPerStrip,
    origin: &'static str,
) -> Result<usize> {
    match (tag, zero) {
        (Some(0), ZeroRowsPerStrip::Refuse) => Err(Error::invalid_input(
            origin,
            "RowsPerStrip must be non-zero",
        )),
        (Some(0) | None, _) => Ok(height),
        (Some(rows), _) => Ok(rows as usize),
    }
}

/// Resolves parallel offset and byte-count arrays into the in-bounds slices of `data` they name,
/// in array order.
///
/// Both arrays are read at full 64-bit width, and `offset + count` is checked rather than summed,
/// so an extent the platform's `usize` cannot express is refused instead of wrapping into one it
/// can.
///
/// # Errors
///
/// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error reported by `origin`
/// when the arrays differ in length, when an extent's end overflows `usize`, or when an extent
/// reaches past the end of `data`.
pub fn extent_slices<'a>(
    offsets: &[u64],
    counts: &[u64],
    data: &'a [u8],
    origin: &'static str,
) -> Result<Vec<&'a [u8]>> {
    if offsets.len() != counts.len() {
        return Err(Error::invalid_input(
            origin,
            "image-data offset/count length mismatch",
        ));
    }
    let mut slices = Vec::with_capacity(offsets.len());
    for (&offset, &count) in offsets.iter().zip(counts) {
        let start = usize::try_from(offset)
            .map_err(|_| Error::invalid_input(origin, "image data out of bounds"))?;
        let end = usize::try_from(count)
            .ok()
            .and_then(|count| start.checked_add(count))
            .ok_or_else(|| Error::invalid_input(origin, "image-data extent overflow"))?;
        slices.push(
            data.get(start..end)
                .ok_or_else(|| Error::invalid_input(origin, "image data out of bounds"))?,
        );
    }
    Ok(slices)
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    const ORIGIN: &str = "test-origin";

    /// Asserts `got` is an `InvalidInput` error from [`ORIGIN`] carrying exactly `message`.
    fn assert_refused<T: std::fmt::Debug>(got: Result<T>, message: &str) {
        let err = got.expect_err("expected an error");
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err:?}");
        assert_eq!(err.static_message(), Some(message), "{err:?}");
        assert_eq!(err.origin(), Some(ORIGIN), "{err:?}");
    }

    /// An absent tag is one full-height strip under either policy. Asserted apart from the zero,
    /// since the mutation gate cannot delete one alternative of the `Some(0) | None` pattern alone.
    #[test]
    fn an_absent_rows_per_strip_is_the_whole_height() {
        for zero in [ZeroRowsPerStrip::WholeImage, ZeroRowsPerStrip::Refuse] {
            assert_eq!(rows_per_strip(None, 3, zero, ORIGIN).expect("absent"), 3);
        }
    }

    /// A zero is the whole height when the policy reads it, and refused when it does not.
    #[test]
    fn a_zero_rows_per_strip_follows_the_policy() {
        assert_eq!(
            rows_per_strip(Some(0), 3, ZeroRowsPerStrip::WholeImage, ORIGIN).expect("read"),
            3
        );
        assert_refused(
            rows_per_strip(Some(0), 3, ZeroRowsPerStrip::Refuse, ORIGIN),
            "RowsPerStrip must be non-zero",
        );
    }

    /// Any other value is returned as stored, under either policy, past the height included.
    #[test]
    fn a_non_zero_rows_per_strip_is_returned_as_stored() {
        for zero in [ZeroRowsPerStrip::WholeImage, ZeroRowsPerStrip::Refuse] {
            assert_eq!(rows_per_strip(Some(2), 3, zero, ORIGIN).expect("two"), 2);
            assert_eq!(
                rows_per_strip(Some(100), 3, zero, ORIGIN).expect("past"),
                100
            );
        }
    }

    #[test]
    fn extent_slices_refuses_arrays_of_different_lengths() {
        assert_refused(
            extent_slices(&[0, 1], &[1], &[0; 4], ORIGIN),
            "image-data offset/count length mismatch",
        );
    }

    /// `offset + count` past `usize::MAX` is an overflow, reported as such rather than wrapped.
    #[test]
    fn extent_slices_refuses_an_extent_that_overflows_usize() {
        let offset = u64::try_from(usize::MAX).expect("usize fits u64");
        assert_refused(
            extent_slices(&[offset], &[1], &[0; 4], ORIGIN),
            "image-data extent overflow",
        );
    }

    /// An extent that ends one byte past the data is out of bounds; one that ends on it is not.
    #[test]
    fn extent_slices_refuses_an_extent_past_the_data() {
        let data = [1, 2, 3, 4];
        assert_refused(
            extent_slices(&[2], &[3], &data, ORIGIN),
            "image data out of bounds",
        );
        assert_eq!(
            extent_slices(&[0, 2], &[2, 2], &data, ORIGIN).expect("in bounds"),
            [&data[..2], &data[2..]]
        );
    }

    /// A classic file's `offset + count` past `u32::MAX` is checked in 64 bits: out of bounds of
    /// a short buffer, never a wrapped extent that lands inside it.
    #[test]
    fn extent_slices_checks_an_extent_past_u32_max_in_64_bits() {
        assert_refused(
            extent_slices(&[0xFFFF_FFF0], &[0x20], &[0; 6], ORIGIN),
            "image data out of bounds",
        );
    }

    /// An offset past `usize::MAX` (reachable only on a target narrower than 64 bits) is out of
    /// bounds rather than an overflow: no slice of memory starts there.
    #[cfg(not(target_pointer_width = "64"))]
    #[test]
    fn extent_slices_refuses_an_offset_past_usize_as_out_of_bounds() {
        assert_refused(
            extent_slices(&[u64::MAX], &[0], &[0; 4], ORIGIN),
            "image data out of bounds",
        );
    }
}
