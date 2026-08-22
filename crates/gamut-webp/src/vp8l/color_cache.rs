//! VP8L color cache (RFC 9649 §3.6.3).
//!
//! The color cache is a small hash table of recently emitted ARGB colors; a pixel can be coded as a
//! short index into it instead of a literal or a backward reference. It is a multiplicative hash
//! with a configurable bit width and **no conflict resolution** — only one slot is checked per
//! color. Both the decoder and the encoder maintain an identical cache and insert **every** produced
//! pixel (literal, copied, or cache hit) in stream order, so their states stay in lock-step.

use gamut_core::{Error, Result};

/// The multiplicative hash constant the spec mandates (RFC 9649 §3.6.3).
const HASH_MULTIPLIER: u32 = 0x1e35_a7bd;

/// Minimum and maximum `color_cache_code_bits` (RFC 9649 §3.6.3).
const MIN_CACHE_BITS: u32 = 1;
pub(crate) const MAX_CACHE_BITS: u32 = 11;

/// A VP8L color cache: `2^bits` slots, each holding one ARGB color, all initialized to zero.
#[derive(Debug, Clone)]
pub struct ColorCache {
    /// The hash width (`color_cache_code_bits`, 1..=11).
    bits: u32,
    /// The slots, indexed by the high `bits` of the multiplicative hash.
    entries: Vec<u32>,
}

impl ColorCache {
    /// Creates a cache with `bits` (1..=11) hash bits and `2^bits` zeroed slots.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `bits` is outside `1..=11`.
    pub fn new(bits: u32) -> Result<Self> {
        if !(MIN_CACHE_BITS..=MAX_CACHE_BITS).contains(&bits) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "VP8L: color cache bits out of range",
            ));
        }
        Ok(Self {
            bits,
            entries: vec![0u32; 1usize << bits],
        })
    }

    /// Number of slots (`2^bits`), which is also the size of the color-cache alphabet.
    #[must_use]
    pub fn size(&self) -> usize {
        self.entries.len()
    }

    /// The hash slot a `color` maps to.
    #[inline]
    #[must_use]
    pub fn slot(&self, color: u32) -> usize {
        (HASH_MULTIPLIER.wrapping_mul(color) >> (32 - self.bits)) as usize
    }

    /// Inserts `color`, overwriting whatever shared its slot.
    pub fn insert(&mut self, color: u32) {
        let slot = self.slot(color);
        if let Some(entry) = self.entries.get_mut(slot) {
            *entry = color;
        }
    }

    /// Returns the color stored at `index` (the decoded cache code). Out-of-range indices yield 0,
    /// but callers bound `index` by [`size`](Self::size) via the green alphabet.
    #[must_use]
    pub fn lookup(&self, index: u32) -> u32 {
        self.entries.get(index as usize).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_range_bits() {
        assert!(ColorCache::new(0).is_err());
        assert!(ColorCache::new(12).is_err());
        assert!(ColorCache::new(1).is_ok());
        assert!(ColorCache::new(11).is_ok());
    }

    #[test]
    fn size_is_two_to_the_bits() {
        assert_eq!(ColorCache::new(1).unwrap().size(), 2);
        assert_eq!(ColorCache::new(10).unwrap().size(), 1024);
    }

    #[test]
    fn insert_then_lookup_round_trips_a_color() {
        let mut cache = ColorCache::new(8).unwrap();
        let color = 0xdead_beef;
        let slot = cache.slot(color);
        assert_eq!(cache.lookup(slot as u32), 0); // empty before insert
        cache.insert(color);
        assert_eq!(cache.lookup(slot as u32), color);
    }

    #[test]
    fn hash_matches_spec_formula() {
        let cache = ColorCache::new(6).unwrap();
        let color = 0x1234_5678;
        let expected = (HASH_MULTIPLIER.wrapping_mul(color) >> (32 - 6)) as usize;
        assert_eq!(cache.slot(color), expected);
        assert!(cache.slot(color) < cache.size());
    }

    #[test]
    fn lookup_out_of_range_is_zero() {
        let cache = ColorCache::new(1).unwrap();
        assert_eq!(cache.lookup(99), 0);
    }
}
