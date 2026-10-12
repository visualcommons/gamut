//! Builds the Adobe DNG SDK 1.7.1 headless from the ZIP in `references/dng/` and links it behind a
//! small `extern "C"` shim.
//!
//! The SDK ships only Visual Studio / Xcode projects, so we compile its `source/*.cpp` directly
//! with the `cc` crate. Three external dependencies the 1.7.1 source assumes are absent on a
//! headless Linux box, so each is replaced by a stub that satisfies the compile/link without
//! changing what the oracle observes (it validates pixel/structure conformance, not metadata):
//!
//! - **Adobe XMP Toolkit** — XMP is kept *enabled* (`qDNGUseXMP=1`, so `dng_metadata`/`dng_xmp`
//!   stay complete and every SDK unit compiles), but its toolkit bridge `dng_xmp_sdk.cpp` is
//!   excluded and replaced by the no-op `src/dng_xmp_sdk_stub.cpp`. The XMP files/doc-ops layers
//!   are off.
//! - **libjpeg** — off (`qDNGUseLibJPEG=0`); lossless JPEG is the SDK's own codec.
//! - **libjxl** — wired in unconditionally by 1.7.1; satisfied by the **real libjxl 0.12.0** that
//!   the `gamut-jxl-sys` dependency statically builds. The SDK is compiled against that exact
//!   build's installed headers (`DEP_JXL_INCLUDE`, published by gamut-jxl-sys's `links = "jxl"`
//!   metadata), and `links = "jxl"` guarantees a single libjxl copy per binary even when a test
//!   executable also links gamut-jxl's encoder — so the oracle genuinely decodes JPEG XL DNGs.
//!   Under `GAMUT_JXL_SYS_SKIP_NATIVE=1` (check-only environments without cmake) gamut-jxl-sys
//!   builds nothing; the SDK then compiles against its vendored libjxl headers with the link-only
//!   stubs in `src/jxl_stub.cpp` — that mode compiles but must never run JXL-touching tests
//!   (nothing links in check-only environments anyway).
//!
//! **zlib** is genuinely required, since the SDK includes `zlib.h` unconditionally for its Deflate
//! and big-table paths. It is the static stock zlib the `zlib-oracle` dependency builds from the
//! `third_party/zlib` submodule: the SDK compiles against that build's headers
//! (`DEP_Z_INCLUDE`, published by zlib-oracle's `links = "z"` metadata) and links its archive.
//! It used to be the system libz (`-lz`), dynamically, which left the one measured path here not
//! built from committed source — the loader picked the library, and cargo's `LD_LIBRARY_PATH`
//! made the pick depend on the launcher (issue #618). One pinned link serves the conformance tests
//! and the benchmark alike; Cargo unifies a package's dev-dependencies across its tests and
//! benches, so they could not link different oracles anyway.
//!
//! Everything lands under `OUT_DIR`, so `cargo clean` fully resets the build. CI needs the ZIP
//! (committed under `references/dng/`), the `third_party/zlib` submodule and a C++ toolchain
//! with cmake; nothing else.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Source files to exclude from the compile: the `main()` driver, the XMP-toolkit bridge (which
/// needs the absent Adobe XMP Toolkit — replaced by the no-op `src/dng_xmp_sdk_stub.cpp`), and the
/// lossless-JPEG shared unit (it is `#include`d by `dng_lossless_jpeg.cpp`, so compiling it
/// separately would duplicate symbols).
const EXCLUDE: &[&str] = &[
    "dng_validate.cpp",
    "dng_xmp_sdk.cpp",
    "dng_lossless_jpeg_shared.cpp",
];

fn main() {
    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env("OUT_DIR"));
    let zip = manifest.join("../../references/dng/dng_sdk_1_7_1_2611_20260609.zip");

    assert!(
        zip.exists(),
        "DNG SDK ZIP not found at {} — it is committed under references/dng/; ensure a full checkout",
        zip.display()
    );

    // ---- Extract the SDK once (guarded by a marker), into OUT_DIR. ------------------------------
    let sdk = out.join("dng_sdk_extracted");
    let base = sdk.join("dng_sdk_1_7_1");
    let source = base.join("dng_sdk").join("source");
    let jxl_include = base
        .join("libjxl")
        .join("libjxl")
        .join("lib")
        .join("include");
    let marker = sdk.join(".extracted");
    if !marker.exists() {
        extract_zip(&zip, &sdk);
        fs::write(&marker, b"ok").expect("write extraction marker");
    }
    assert!(
        source.join("dng_negative.cpp").exists(),
        "extracted SDK source not found under {}",
        source.display()
    );

    silence_intel_compiler_note(&source.join("dng_simd_type.h"));

    // libjxl's public headers `#include` CMake-generated export-macro headers; the gamut-jxl-sys
    // install tree has the real generated ones, but the SDK's vendored source tree does not, so
    // empty-macro stubs back both modes (static build, no symbol visibility decoration).
    let jxl_shim = out.join("jxl_shim");
    write_jxl_export_stubs(&jxl_shim.join("jxl"));

    // The libjxl headers to compile the SDK against: the exact installed headers of the
    // gamut-jxl-sys static build when it ran (`DEP_JXL_INCLUDE`), else — check-only skip-native
    // mode — the SDK's vendored copy, with `src/jxl_stub.cpp` satisfying the link.
    let real_libjxl = std::env::var_os("DEP_JXL_INCLUDE").map(PathBuf::from);

    // ---- Compile the SDK + shim (+ libjxl stubs in skip-native mode) into one archive. ----------
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17");
    build.include(&source);
    match &real_libjxl {
        Some(include) => {
            build.include(include);
        }
        None => {
            build.include(&jxl_include);
        }
    }
    build.include(&jxl_shim);
    // The pinned zlib's own headers, ahead of any system `<zlib.h>` (see the module docs).
    let zlib_include = env("DEP_Z_INCLUDE");
    for dir in std::env::split_paths(&zlib_include) {
        build.include(dir);
    }
    // Headless Linux, little-endian. XMP is *enabled* (so `dng_metadata`/`dng_xmp` stay complete
    // and the SDK source compiles cleanly) but its toolkit bridge is the no-op
    // `src/dng_xmp_sdk_stub.cpp`; the XMP files/doc-ops layers are off (nothing references them).
    // libjpeg and threading are off (lossless JPEG is the SDK's own codec). The
    // platform/endianness/64-bit macros resolve automatically once qLinux=1 is set.
    for (k, v) in [
        ("qLinux", "1"),
        ("qDNGUseXMP", "1"),
        ("qDNGXMPFiles", "0"),
        ("qDNGXMPDocOps", "0"),
        ("qDNGUseLibJPEG", "0"),
        ("qDNGThreadSafe", "0"),
        ("qDNGValidate", "0"),
        ("qDNGValidateTarget", "0"),
    ] {
        build.define(k, v);
    }
    // The vendored SDK is warning-heavy; it is reference code we do not own, so silence warnings
    // rather than treat them as signal.
    build.flag_if_supported("-w");

    let exclude: HashSet<&str> = EXCLUDE.iter().copied().collect();
    let mut count = 0;
    for entry in fs::read_dir(&source).expect("read SDK source dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("cpp") {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !exclude.contains(name) {
                build.file(&path);
                count += 1;
            }
        }
    }
    assert!(
        count > 50,
        "expected the full SDK source set, found only {count} files"
    );

    build.file(manifest.join("src/oracle_shim.cpp"));
    // With the real libjxl linked (via gamut-jxl-sys), the stubs must NOT be compiled — they
    // would collide with the genuine symbols. They exist solely for the check-only skip-native
    // mode.
    if real_libjxl.is_none() {
        build.file(manifest.join("src/jxl_stub.cpp"));
    }
    build.file(manifest.join("src/dng_xmp_sdk_stub.cpp"));
    // Build parallelism: `cc::Build` spawns NUM_JOBS compilers — and this is the 50+ C++17
    // translation units of the Adobe DNG SDK — while cargo derives NUM_JOBS from
    // `--jobs`/`CARGO_BUILD_JOBS`. That is the same value `tooling/build-env`'s
    // `build_parallelism` falls back to, so one `CARGO_BUILD_JOBS` bounds this compile and the
    // cmake/ninja oracles together; there is deliberately no dial here.
    build.compile("dng_oracle");

    // The SDK includes <zlib.h> unconditionally (Deflate + big-table compression). Nothing is
    // linked for it here: `zlib-oracle` emits the static `libz.a`, and `src/lib.rs` names that
    // crate so rustc links it after this archive.

    // The Adobe sample DNGs shipped in the SDK ZIP, for decode-conformance tests.
    println!(
        "cargo:rustc-env=GDNG_SAMPLE_FILES_DIR={}",
        base.join("sample_files").display()
    );

    println!("cargo:rerun-if-changed=src/oracle_shim.cpp");
    println!("cargo:rerun-if-changed=src/jxl_stub.cpp");
    println!("cargo:rerun-if-changed=src/dng_xmp_sdk_stub.cpp");
    println!("cargo:rerun-if-changed=build.rs");
}

/// Extracts every entry of the ZIP at `zip` under `dest`.
fn extract_zip(zip: &Path, dest: &Path) {
    let file = fs::File::open(zip).expect("open DNG SDK ZIP");
    let mut archive = zip::ZipArchive::new(file).expect("read DNG SDK ZIP");
    archive.extract(dest).expect("extract DNG SDK ZIP");
}

/// Neutralizes the SDK's `INTEL_COMPILER_NEEDED_NOTE` macro in the extracted `dng_simd_type.h`.
///
/// On a non-Intel x86 compiler that is neither clang nor MSVC — i.e. GCC here — the SDK expands
/// that macro to `_Pragma("message(...)")` at ~30 call sites, so every build prints a screenful of
/// "Intel Compiler needed for optimizations" notes. They are advisory only (the macro is already
/// empty on the Intel, clang, and macOS paths), and `#pragma message` output is a *note*, not a
/// warning, so the `-w` on the compile does not suppress it and GCC offers no flag that does.
/// Appending an unconditional empty redefinition after the header's own `#define` is idempotent
/// (the lines sit outside the include guard, so they re-apply on every inclusion) and confined to
/// the `OUT_DIR` copy, which `cargo clean` discards.
fn silence_intel_compiler_note(header: &Path) {
    const SENTINEL: &str = "/* gamut: Intel-compiler advisory notes silenced */";
    let mut text = fs::read_to_string(header).expect("read dng_simd_type.h");
    if text.contains(SENTINEL) {
        return;
    }
    text.push_str(&format!(
        "\n{SENTINEL}\n#undef INTEL_COMPILER_NEEDED_NOTE\n#define INTEL_COMPILER_NEEDED_NOTE\n"
    ));
    fs::write(header, text).expect("write patched dng_simd_type.h");
}

/// Writes empty-macro stubs for the CMake-generated libjxl export headers under `dir`.
fn write_jxl_export_stubs(dir: &Path) {
    fs::create_dir_all(dir).expect("create jxl export-stub dir");
    for (file, prefix) in [
        ("jxl_export.h", "JXL"),
        ("jxl_threads_export.h", "JXL_THREADS"),
        ("jxl_cms_export.h", "JXL_CMS"),
    ] {
        let content = format!(
            "#ifndef {p}_EXPORT_H\n#define {p}_EXPORT_H\n\
             #define {p}_EXPORT\n#define {p}_NO_EXPORT\n#define {p}_DEPRECATED\n\
             #define {p}_DEPRECATED_EXPORT\n#define {p}_DEPRECATED_NO_EXPORT\n#endif\n",
            p = prefix
        );
        fs::write(dir.join(file), content).expect("write jxl export stub");
    }
}

/// Reads a required build-time env var, panicking (this is a build script) if absent.
fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("missing build env var {key}"))
}
