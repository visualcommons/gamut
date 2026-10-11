//! CRC-32 for PNG chunk integrity (PNG spec §5.3, the ISO-3309 / ITU-T V.42 polynomial).
//!
//! This is the reflected CRC-32 with polynomial `0xEDB88320`, initial value all-ones, and a final
//! ones-complement, computed over a chunk's **type and data** (not its length). zlib uses Adler-32,
//! never this — so CRC-32 lives in the PNG crate, not in `gamut-deflate`.
//!
//! The arithmetic is [`crc32fast`]'s; this module is the PNG-shaped wrapper over it. The tests
//! below stay as a drift guard: they pin the polynomial this file's doc claims, so swapping the
//! backend for one computing a different CRC-32 variant (Castagnoli, say) fails here rather than
//! silently producing files no decoder accepts.

/// An incremental CRC-32 accumulator.
///
/// Delegates to [`crc32fast`], which dispatches to PCLMULQDQ/AVX-512 on x86-64 and the `crc32`
/// instructions on aarch64, falling back to a table elsewhere (wasm32 included). The `unsafe`
/// that needs is entirely inside that crate; nothing here changes.
///
/// This runs over every byte of every chunk, IDAT included, so it is on the critical path of
/// every encode. The byte-at-a-time table loop it replaces managed roughly 420 MB/s.
pub struct Crc32(crc32fast::Hasher);

impl Crc32 {
    /// Starts a fresh CRC (register initialised to all ones).
    // No `Default` impl to pair with this: nothing in the crate would call it, so it would be an
    // uncovered region and an unkillable mutant -- a delegation no test can reach. `new` is only
    // `pub` so `crate::stages` can re-export it to the benchmark driver.
    // `allow`, not `expect`: the lint only fires when `test-support` re-exports this type through
    // `crate::stages`, so an `expect` is unfulfilled in a default-feature build and fails there
    // instead. A `Default` impl would be dead delegation -- nothing in the crate calls it, so it
    // would be an uncovered region and a mutant no test could kill.
    #[allow(
        clippy::new_without_default,
        reason = "a Default impl here would be dead delegation: uncovered, and unkillable by any test"
    )]
    pub fn new() -> Self {
        Self(crc32fast::Hasher::new())
    }

    /// Folds `data` into the running CRC.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// Finalises the CRC (ones-complement of the register).
    pub fn finish(self) -> u32 {
        self.0.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc(data: &[u8]) -> u32 {
        let mut c = Crc32::new();
        c.update(data);
        c.finish()
    }

    #[test]
    fn known_chunk_crcs() {
        // The CRC over the bytes "IEND" is the fixed value every PNG's end chunk carries.
        assert_eq!(crc(b"IEND"), 0xAE42_6082);
        // CRC of the empty string is 0.
        assert_eq!(crc(b""), 0);
    }

    #[test]
    fn incremental_matches_one_shot() {
        // Folding in two parts equals folding the whole (chunk writers feed type then data).
        let mut split = Crc32::new();
        split.update(b"IHDR");
        split.update(&[0, 0, 1, 0]);
        let mut whole = Crc32::new();
        whole.update(b"IHDR\x00\x00\x01\x00");
        assert_eq!(split.finish(), whole.finish());
    }
}
