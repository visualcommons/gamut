//! Dev-only DNG conformance oracle around a headless-built **Adobe DNG SDK 1.7.1**.
//!
//! gamut-dng's encoder must produce files the canonical reference implementation accepts.
//! [`validate_dng`] writes the bytes to a temporary file and runs the SDK's parse → build-negative
//! → read-stage-1 flow (the same one its `dng_validate` tool uses); it succeeds only if the SDK
//! reads the file without error. All `unsafe` FFI is confined to this crate.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};

// Never called from Rust, but required in the dependency graph: it statically builds and links
// the real libjxl the SDK's JPEG XL reader calls, and publishes the matching header path to
// build.rs (`links = "jxl"`).
use gamut_jxl_sys as _;

unsafe extern "C" {
    /// Returns `0` if the Adobe DNG SDK validates the DNG at `path`, else its error code.
    fn gdng_validate(path: *const c_char) -> c_int;

    /// Reads the stage-1 raw samples of the DNG at `path` into a freshly allocated `uint16` buffer
    /// (`width * height * planes`); `0` on success, else the SDK error code. Free with `gdng_free`.
    fn gdng_read_raw(
        path: *const c_char,
        out_w: *mut u32,
        out_h: *mut u32,
        out_planes: *mut u32,
        out_data: *mut *mut u16,
        out_len: *mut usize,
    ) -> c_int;

    /// Reads the stage-2 (linearized) image of the DNG at `path` — the SDK's application of the
    /// DNG Chapter-5 raw-to-linear mapping — into a freshly allocated `uint16` buffer
    /// (active-area `width * height * planes`, `0..=65535` encoding linear `0.0..=1.0`);
    /// `0` on success, else the SDK error code. Free with `gdng_free`.
    fn gdng_read_linear(
        path: *const c_char,
        out_w: *mut u32,
        out_h: *mut u32,
        out_planes: *mut u32,
        out_data: *mut *mut u16,
        out_len: *mut usize,
    ) -> c_int;

    /// Decodes a bare lossless-JPEG (SOF3) stream with the SDK's own codec into a freshly
    /// allocated interleaved `uint16` buffer of exactly `expected_samples`; `0` on success,
    /// else the SDK error code. Free with `gdng_free`.
    fn gdng_decode_lossless_jpeg(
        data: *const u8,
        len: usize,
        expected_samples: usize,
        out_data: *mut *mut u16,
        out_len: *mut usize,
    ) -> c_int;

    /// Decodes the same bare lossless-JPEG stream as `gdng_decode_lossless_jpeg` but stops at
    /// the spooler, reporting only how many samples the SDK produced; `0` on success, else the
    /// SDK error code. Nothing is allocated for the caller, so there is nothing to free.
    fn gdng_decode_lossless_jpeg_extent(
        data: *const u8,
        len: usize,
        expected_samples: usize,
        out_len: *mut usize,
    ) -> c_int;

    /// Decodes the DNG in `data`/`len` from memory and reports the stage-1 image's geometry and
    /// sample count without exporting the samples; `0` on success, else the SDK error code.
    /// Nothing is allocated for the caller, so there is nothing to free.
    fn gdng_decode_dng_in_memory(
        data: *const u8,
        len: usize,
        out_w: *mut u32,
        out_h: *mut u32,
        out_planes: *mut u32,
        out_len: *mut usize,
    ) -> c_int;

    /// Returns the identity of the zlib the SDK's Deflate reader calls: its version, and where
    /// the loader found it. Static storage duration; valid for the process.
    fn gdng_zlib_identity() -> *const c_char;

    /// Returns the resolved path of that same zlib alone, or null when the loader cannot report
    /// one. Static storage duration; valid for the process.
    fn gdng_zlib_path() -> *const c_char;

    /// Computes the SDK's `NewRawImageDigest` for the DNG at `path` into `out_digest` (16 bytes);
    /// `0` on success, else the SDK error code.
    fn gdng_new_raw_image_digest(path: *const c_char, out_digest: *mut u8) -> c_int;

    /// Derives the camera neutral for the DNG at `path` from its `AsShotWhiteXY` tag, writing
    /// `*out_channels` (at most 4) doubles to `out_neutral`; `0` on success, else the SDK error
    /// code (`dng_error_bad_format` if the file carries no `AsShotWhiteXY`).
    fn gdng_camera_neutral_from_white_xy(
        path: *const c_char,
        out_channels: *mut u32,
        out_neutral: *mut f64,
    ) -> c_int;

    /// Releases a buffer returned by [`gdng_read_raw`] / [`gdng_read_linear`] /
    /// [`gdng_decode_lossless_jpeg`].
    fn gdng_free(data: *mut u16);
}

/// The directory of Adobe's official sample DNGs shipped inside the SDK ZIP (extracted at build
/// time) — JPEG XL raws, ProfileGainTableMap2 variants, ImageSequenceInfo/ImageStats carriers.
#[must_use]
pub fn sample_files_dir() -> &'static Path {
    Path::new(env!("GDNG_SAMPLE_FILES_DIR"))
}

/// Reads one of Adobe's sample DNGs by file name (e.g. `"01_jxl_linear_raw_integer.dng"`).
///
/// # Errors
///
/// Returns an error message if the file cannot be read.
pub fn sample_file(name: &str) -> Result<Vec<u8>, String> {
    let path: PathBuf = sample_files_dir().join(name);
    std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Computes the Adobe SDK's **`NewRawImageDigest`** — its MD5-over-raw-image algorithm
/// (`dng_negative::FindNewRawImageDigest`) — for `bytes`, the reference for gamut-dng's digest
/// writer.
///
/// # Errors
///
/// Returns an error message if the bytes cannot be written to a temporary file or the SDK cannot
/// read the file (with its numeric error code).
pub fn new_raw_image_digest(bytes: &[u8]) -> Result<[u8; 16], String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.dng");
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let cpath =
        CString::new(path.to_str().ok_or("non-UTF-8 temp path")?).map_err(|e| e.to_string())?;
    let mut digest = [0u8; 16];
    // SAFETY: `cpath` is a valid NUL-terminated path; `digest` provides the 16 writable bytes the
    // entry point fills.
    let code = unsafe { gdng_new_raw_image_digest(cpath.as_ptr(), digest.as_mut_ptr()) };
    if code == 0 {
        Ok(digest)
    } else {
        Err(format!(
            "Adobe DNG SDK could not compute NewRawImageDigest (error code {code})"
        ))
    }
}

/// A 16-bit image as the Adobe DNG SDK reads it: interleaved samples and their geometry — the
/// stage-1 sensor values from [`read_raw_dng`], or the stage-2 linear encoding from
/// [`read_linear_dng`].
#[derive(Debug, Clone)]
pub struct AdobeRaw {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Colour planes per pixel.
    pub planes: u32,
    /// Interleaved samples, row-major, `width * height * planes` long.
    pub samples: Vec<u16>,
}

/// Validates `bytes` as a DNG with the Adobe DNG SDK.
///
/// Returns `Ok(())` if the SDK parses the directories, builds a negative, reads the raw image
/// without error, **and** any stored `RawImageDigest`/`NewRawImageDigest` matches the image data
/// (the SDK records a mismatch as "damaged", surfaced here as error code 1); otherwise `Err`
/// with the error code.
///
/// # Errors
///
/// Returns an error message if the bytes cannot be written to a temporary file, or if the SDK
/// rejects the file (with its numeric error code).
pub fn validate_dng(bytes: &[u8]) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.dng");
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let cpath =
        CString::new(path.to_str().ok_or("non-UTF-8 temp path")?).map_err(|e| e.to_string())?;
    // SAFETY: `cpath` is a valid NUL-terminated path; the SDK only opens and reads the file at it.
    let code = unsafe { gdng_validate(cpath.as_ptr()) };
    if code == 0 {
        Ok(())
    } else {
        Err(format!(
            "Adobe DNG SDK rejected the file (error code {code})"
        ))
    }
}

/// Runs one of the SDK's image-reading entry points over `bytes` written to a temporary file.
fn read_image(
    bytes: &[u8],
    // SAFETY contract: an `extern "C"` shim entry with the shared
    // `(path, w, h, planes, data, len)` signature that fills a `malloc`d `u16` buffer.
    entry: unsafe extern "C" fn(
        *const c_char,
        *mut u32,
        *mut u32,
        *mut u32,
        *mut *mut u16,
        *mut usize,
    ) -> c_int,
    what: &str,
) -> Result<AdobeRaw, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.dng");
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let cpath =
        CString::new(path.to_str().ok_or("non-UTF-8 temp path")?).map_err(|e| e.to_string())?;

    let (mut w, mut h, mut planes): (u32, u32, u32) = (0, 0, 0);
    let mut data: *mut u16 = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `cpath` is a valid NUL-terminated path; on success `data`/`len` describe a buffer the
    // SDK allocated with `malloc`, which we copy out of and then release with `gdng_free`.
    let code = unsafe {
        entry(
            cpath.as_ptr(),
            &mut w,
            &mut h,
            &mut planes,
            &mut data,
            &mut len,
        )
    };
    if code != 0 || data.is_null() {
        return Err(format!(
            "Adobe DNG SDK could not read the {what} image (code {code})"
        ));
    }
    // SAFETY: `data` points at `len` `u16`s the SDK just allocated; copy then free.
    let samples = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    unsafe { gdng_free(data) };
    Ok(AdobeRaw {
        width: w,
        height: h,
        planes,
        samples,
    })
}

/// Derives the camera neutral the Adobe DNG SDK computes for `bytes` from its `AsShotWhiteXY`
/// (50729) tag — the DNG 1.7.1 "Translating White Balance xy Coordinates to Camera Neutral
/// Coordinates" conversion, as the reference implementation performs it.
///
/// The SDK honours `AsShotWhiteXY` only when `AsShotNeutral` (50728) is absent, so this rejects a
/// file carrying the neutral instead of quietly answering for the wrong tag.
///
/// # Errors
///
/// Returns an error message if the bytes cannot be written to a temporary file, or if the SDK
/// cannot derive a neutral from them (with its numeric error code).
pub fn camera_neutral_from_white_xy(bytes: &[u8]) -> Result<Vec<f64>, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("oracle.dng");
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let cpath =
        CString::new(path.to_str().ok_or("non-UTF-8 temp path")?).map_err(|e| e.to_string())?;

    let mut channels: u32 = 0;
    // `kMaxColorPlanes` in the SDK; the shim refuses to write more.
    let mut neutral = [0.0f64; 4];
    // SAFETY: `cpath` is a valid NUL-terminated path; the shim writes at most `neutral.len()`
    // doubles and reports how many in `channels`.
    let code = unsafe {
        gdng_camera_neutral_from_white_xy(cpath.as_ptr(), &mut channels, neutral.as_mut_ptr())
    };
    if code != 0 {
        return Err(format!(
            "Adobe DNG SDK could not derive a camera neutral from AsShotWhiteXY (code {code})"
        ));
    }
    let channels = usize::try_from(channels).map_err(|e| e.to_string())?;
    neutral
        .get(..channels)
        .map(<[f64]>::to_vec)
        .ok_or_else(|| format!("Adobe DNG SDK reported {channels} colour channels"))
}

/// Reads `bytes` as a DNG with the Adobe DNG SDK and returns its stage-1 raw samples.
///
/// # Errors
///
/// Returns an error message if the bytes cannot be written to a temporary file, or if the SDK
/// cannot read the raw image (with its numeric error code).
pub fn read_raw_dng(bytes: &[u8]) -> Result<AdobeRaw, String> {
    read_image(bytes, gdng_read_raw, "raw")
}

/// Reads `bytes` as a DNG with the Adobe DNG SDK and returns its **stage-2 (linearized)** image:
/// the SDK's application of the DNG Chapter-5 "Mapping Raw Values to Linear Reference Values"
/// pipeline (linearization table, black subtraction with deltas, rescale, clip).
///
/// The result is **active-area-sized** and encodes linear `0.0..=1.0` as `0..=65535`
/// (`linear = samples[i] as f64 / 65535.0`); the default SDK host preserves no black levels, so
/// `0` is black.
///
/// # Errors
///
/// Returns an error message if the bytes cannot be written to a temporary file, or if the SDK
/// cannot build/read the stage-2 image (with its numeric error code).
pub fn read_linear_dng(bytes: &[u8]) -> Result<AdobeRaw, String> {
    read_image(bytes, gdng_read_linear, "stage-2 linear")
}

/// Identifies the zlib the SDK's Deflate reader calls: its `zlibVersion()` string and, where the
/// loader can report it, the resolved path of the shared object the symbol came from — e.g.
/// `"1.3.1 from /usr/lib64/libz.so.1.3.1"`.
///
/// The path is the discriminating part: `zlibVersion()` reports the string the loaded build
/// carries, so it separates the copy a build script left under `target/` from the platform's only
/// when the platform's build renamed itself. Two stock builds of one version — which is what a box
/// shipping stock zlib gives — carry the same string, and the path still tells them apart.
///
/// `build.rs` links the system libz dynamically (`-lz`) because the SDK includes `<zlib.h>`
/// unconditionally. That makes the SDK's Deflate decode the one measured path in this oracle that
/// is **not** built from source committed to this repository: which libz the dynamic linker
/// resolves is a property of the machine, and inflate implementations differ by far more than the
/// margin that decides whether gamut or the reference implementation is faster on a Deflate row.
/// `cargo bench -p gamut-dng --bench codec` prints this next to its fixture table so a Deflate
/// ratio is never published without the library it is a ratio against.
///
/// Falls back to `"unknown"` if libz returns no string, which it is not documented to do.
#[must_use]
pub fn zlib_identity() -> String {
    // SAFETY: the shim returns a pointer to a NUL-terminated string with static storage duration,
    // valid for the life of the process; the `CStr` borrow ends before this function returns.
    let raw = unsafe { gdng_zlib_identity() };
    if raw.is_null() {
        return "unknown".to_string();
    }
    // SAFETY: non-null, and as above NUL-terminated and static.
    unsafe { CStr::from_ptr(raw) }
        .to_str()
        .unwrap_or("unknown")
        .to_string()
}

/// The resolved path of that libz on its own — the same string [`zlib_identity`] appends, handed
/// over unformatted so a caller can *test* it rather than print it.
///
/// Returns `None` when the loader reports no path, or reports one that is not UTF-8.
///
/// A caller that finds this path inside a Cargo build directory has learned something the version
/// string cannot tell it: the loader resolved libz from the build graph rather than from the
/// platform. `cargo` puts every build script's native search path on `LD_LIBRARY_PATH`, and
/// `gamut-dng`'s own dev-dependency `libtiff-oracle` builds a `libz.so` under `target/`, so a
/// benchmark launched through `cargo` can measure a different inflate implementation from the one
/// the same binary measures when run directly. That resolution is not reproducible for anyone
/// else, which is why `cargo bench -p gamut-dng --bench codec` flags it rather than only printing
/// it.
#[must_use]
pub fn zlib_path() -> Option<PathBuf> {
    // SAFETY: the shim returns either null or a pointer to a NUL-terminated string with static
    // storage duration, valid for the life of the process; the `CStr` borrow ends here.
    let raw = unsafe { gdng_zlib_path() };
    if raw.is_null() {
        return None;
    }
    // SAFETY: non-null, and as above NUL-terminated and static.
    let text = unsafe { CStr::from_ptr(raw) }.to_str().ok()?;
    Some(PathBuf::from(text))
}

/// The extent of an image the Adobe DNG SDK decoded: its geometry and how many samples it holds.
///
/// Deliberately carries no pixels. It is what [`decode_dng_in_memory`] reports, and the point of
/// that entry is to *not* pay for an export copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedExtent {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Colour planes per pixel.
    pub planes: u32,
    /// How many samples the decoded image holds: `width * height * planes`.
    pub samples: usize,
}

/// Decodes `bytes` as a DNG with the Adobe DNG SDK **from memory**, returning only the extent of
/// the stage-1 raw image it produced.
///
/// This is the reference implementation's decode reduced to the work gamut's
/// `DngDecoder::decode` also does, so the two can be timed against each other
/// (`cargo bench -p gamut-dng --bench codec`):
///
/// - no temporary file is written and no `dng_file_stream` is opened — the SDK parses the same
///   in-memory bytes the Rust caller holds; and
/// - the decoded image is not exported into a caller-owned buffer, so the extra full-image
///   `malloc` + `memcpy` that [`read_raw_dng`] must perform to cross the FFI boundary is not
///   charged to the codec.
///
/// Use [`read_raw_dng`] when you want the samples; this one when you want the time.
///
/// That the two agree is pinned by `adobe_in_memory_decode_matches_the_file_decode` in
/// `gamut-dng`'s `tests/roundtrip.rs`, not here: this crate is excluded from the workspace, so a
/// test in it never runs in automation.
///
/// # Errors
///
/// Returns an error message (with the SDK's numeric error code) if the SDK cannot parse the bytes
/// or read the raw image.
pub fn decode_dng_in_memory(bytes: &[u8]) -> Result<DecodedExtent, String> {
    let (mut width, mut height, mut planes): (u32, u32, u32) = (0, 0, 0);
    let mut samples: usize = 0;
    // SAFETY: `bytes` outlives the call and the shim only reads `bytes.len()` bytes from it; the
    // four out-parameters are distinct live locals and the shim allocates nothing for us.
    let code = unsafe {
        gdng_decode_dng_in_memory(
            bytes.as_ptr(),
            bytes.len(),
            &mut width,
            &mut height,
            &mut planes,
            &mut samples,
        )
    };
    if code != 0 {
        return Err(format!(
            "Adobe DNG SDK could not decode the DNG from memory (code {code})"
        ));
    }
    Ok(DecodedExtent {
        width,
        height,
        planes,
        samples,
    })
}

/// Decodes a **bare lossless-JPEG (SOF3) stream** with the Adobe DNG SDK's own codec — the
/// reference for gamut-dng's T.81 process-14 decoder (predictors 1–7, point transform,
/// row-aligned restart intervals).
///
/// `expected_samples` is `width * height * components`; the SDK spools exactly that many
/// interleaved `u16` samples on success.
///
/// # Errors
///
/// Returns an error message (with the SDK's numeric error code) if the SDK rejects the stream
/// or decodes a different sample count.
pub fn decode_lossless_jpeg(stream: &[u8], expected_samples: usize) -> Result<Vec<u16>, String> {
    let mut data: *mut u16 = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `stream` outlives the call; on success `data`/`len` describe a `malloc`d buffer we
    // copy out of and then release with `gdng_free`.
    let code = unsafe {
        gdng_decode_lossless_jpeg(
            stream.as_ptr(),
            stream.len(),
            expected_samples,
            &mut data,
            &mut len,
        )
    };
    if code != 0 || data.is_null() {
        return Err(format!(
            "Adobe DNG SDK could not decode the lossless JPEG (code {code})"
        ));
    }
    // SAFETY: `data` points at `len` `u16`s the SDK just allocated; copy then free.
    let samples = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    unsafe { gdng_free(data) };
    Ok(samples)
}

/// Decodes the same bare lossless-JPEG (SOF3) stream as [`decode_lossless_jpeg`] with the Adobe
/// DNG SDK, but **without exporting the samples** — it returns how many the SDK produced.
///
/// The two entry points run the identical `DecodeLosslessJPEG<Scalar>` call into the identical
/// spool buffer and differ only in what happens afterwards: [`decode_lossless_jpeg`] must
/// `malloc` a buffer, `memcpy` the spool into it and copy that into a `Vec` to cross the FFI
/// boundary, and this one does none of those. Timing the pair therefore measures the export path
/// and nothing else, which is how `cargo bench -p gamut-dng --bench codec` quantifies the one
/// residual bias in its codestream comparison instead of merely asserting it is small.
///
/// That the two agree is pinned by `sdk_extent_entry_counts_the_same_samples_as_the_exporting_entry`
/// in `gamut-dng`'s `lossless_jpeg` tests, not here, for the reason given on
/// [`decode_dng_in_memory`].
///
/// # Errors
///
/// Returns an error message (with the SDK's numeric error code) if the SDK cannot decode the
/// stream, or if it produces a different number of samples than `expected_samples`.
pub fn decode_lossless_jpeg_extent(
    stream: &[u8],
    expected_samples: usize,
) -> Result<usize, String> {
    let mut len: usize = 0;
    // SAFETY: `stream` outlives the call and the shim only reads `stream.len()` bytes from it;
    // `len` is a live local and the shim allocates nothing for us.
    let code = unsafe {
        gdng_decode_lossless_jpeg_extent(stream.as_ptr(), stream.len(), expected_samples, &mut len)
    };
    if code != 0 {
        return Err(format!(
            "Adobe DNG SDK could not decode the lossless JPEG (code {code})"
        ));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the SDK links and runs, and rejects non-DNG bytes without crashing.
    #[test]
    fn rejects_non_dng_bytes() {
        assert!(validate_dng(b"this is not a DNG file").is_err());
        assert!(read_raw_dng(b"this is not a DNG file").is_err());
        assert!(decode_lossless_jpeg(b"not a JPEG", 4).is_err());
        assert!(new_raw_image_digest(b"this is not a DNG file").is_err());
    }

    /// The linked libjxl is real (not the check-only stubs): the SDK decodes Adobe's own
    /// JPEG-XL-compressed sample DNG to actual pixels.
    #[test]
    fn decodes_adobe_jxl_sample_via_real_libjxl() {
        let bytes = sample_file("01_jxl_linear_raw_integer.dng").expect("sample DNG present");
        let raw = read_raw_dng(&bytes).expect("JXL DNG must decode through real libjxl");
        assert!(raw.width > 0 && raw.height > 0);
        assert_eq!(raw.planes, 3, "linear-raw sample has 3 planes");
        assert!(
            raw.samples.iter().any(|&s| s != 0),
            "stub libjxl would leave the image all-zero"
        );
    }

    /// The digest entry point computes a stable, non-trivial MD5 for a real file.
    #[test]
    fn computes_new_raw_image_digest_for_sample() {
        let bytes = sample_file("05_PGTM2_unsigned8.dng").expect("sample DNG present");
        let digest = new_raw_image_digest(&bytes).expect("digest");
        assert_ne!(digest, [0u8; 16]);
        // Deterministic across calls.
        assert_eq!(digest, new_raw_image_digest(&bytes).expect("digest again"));
    }
}
