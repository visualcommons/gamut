//! Compiles jpegli (libjxl v0.11.2's `lib/jpegli`, from the `jpegxl-src` 0.11.4 package) and the
//! highway runtime it dispatches through, plus the `src/shim.cc` bridge, into one static archive
//! under `OUT_DIR`. Everything is driven by the `cc` crate — no CMake — so, like `lcms2-oracle`,
//! the build spawns no tool of its own and has no ambient launcher env for `build-env` to
//! normalise; `cc` resolves the compiler the same way for every oracle that uses it.
//!
//! The source list is libjxl v0.11.2's `JPEGXL_INTERNAL_JPEGLI_SOURCES` (`lib/jxl_lists.cmake`)
//! minus its headers, and highway's runtime translation units minus tests and benchmarks.

use std::path::{Path, PathBuf};

/// Must match the exact `jpegxl-src = "=0.11.4"` pin in Cargo.toml: the build locates the vendored
/// libjxl tree by this version string.
const JPEGXL_SRC_VERSION: &str = "0.11.4";

/// libjxl v0.11.2's `JPEGXL_INTERNAL_JPEGLI_SOURCES`, translation units only.
const JPEGLI_SOURCES: &[&str] = &[
    "adaptive_quantization.cc",
    "bit_writer.cc",
    "bitstream.cc",
    "color_quantize.cc",
    "color_transform.cc",
    "common.cc",
    "decode.cc",
    "decode_marker.cc",
    "decode_scan.cc",
    "destination_manager.cc",
    "downsample.cc",
    "encode.cc",
    "encode_finish.cc",
    "encode_streaming.cc",
    "entropy_coding.cc",
    "error.cc",
    "huffman.cc",
    "idct.cc",
    "input.cc",
    "memory_manager.cc",
    "quant.cc",
    "render.cc",
    "simd.cc",
    "source_manager.cc",
    "upsample.cc",
];

/// highway's runtime (target detection and dynamic dispatch, aligned allocation, abort/print).
const HWY_SOURCES: &[&str] = &[
    "abort.cc",
    "aligned_allocator.cc",
    "per_target.cc",
    "print.cc",
    "targets.cc",
    "timer.cc",
];

fn main() {
    let manifest_dir = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env("OUT_DIR"));
    let libjxl = libjxl_dir();
    let turbo = manifest_dir.join("../../third_party/libjpeg-turbo/src");
    assert!(
        turbo.join("jpeglib.h").exists(),
        "libjpeg-turbo headers not found under {} — run `git submodule update --init \
         third_party/libjpeg-turbo`",
        turbo.display()
    );

    // ---- libjpeg headers for jpegli's renamed libjpeg-6b API. ---------------------------------
    // jpegli includes `<jpeglib.h>`, which pulls in `jconfig.h` and `jmorecfg.h` from its own
    // directory first; staging all three together keeps the submodule's headers from finding a
    // stray `jconfig.h` anywhere else. The configuration mirrors libjxl's `jpegli.cmake` with its
    // default `JPEGLI_LIBJPEG_LIBRARY_SOVERSION = 62`.
    let include = out_dir.join("include");
    std::fs::create_dir_all(&include).expect("create include dir");
    for header in ["jpeglib.h", "jmorecfg.h"] {
        std::fs::copy(turbo.join(header), include.join(header)).expect("stage libjpeg header");
    }
    std::fs::write(
        include.join("jconfig.h"),
        "#define JPEG_LIB_VERSION 62\n\
         #define LIBJPEG_TURBO_VERSION 0.0.0\n\
         #define LIBJPEG_TURBO_VERSION_NUMBER 0\n\
         #define MEM_SRCDST_SUPPORTED 1\n\
         #ifndef BITS_IN_JSAMPLE\n#define BITS_IN_JSAMPLE 8\n#endif\n",
    )
    .expect("write jconfig.h");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        // The oracle runs inside debug test binaries, where `cc` would otherwise compile at -O0;
        // jpegli's SIMD paths at -O0 make every Kodak-sized encode take seconds.
        .opt_level(2)
        .include(&libjxl)
        .include(libjxl.join("lib/include"))
        .include(libjxl.join("third_party/highway"))
        .include(&include)
        .define("HWY_STATIC_DEFINE", None)
        .flag_if_supported("-w");
    for file in JPEGLI_SOURCES {
        build.file(libjxl.join("lib/jpegli").join(file));
    }
    for file in HWY_SOURCES {
        build.file(libjxl.join("third_party/highway/hwy").join(file));
    }
    build.file(manifest_dir.join("src/shim.cc"));
    build.compile("jpegli_oracle");

    println!("cargo:rerun-if-changed=src/shim.cc");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=JPEGLI_ORACLE_LIBJXL_DIR");
    println!(
        "cargo:rerun-if-changed={}",
        path_str(&turbo.join("jpeglib.h"))
    );
}

/// The libjxl v0.11.2 source tree inside the `jpegxl-src` 0.11.4 package.
///
/// A build script cannot ask Cargo where a build-dependency's sources live, so this follows
/// gamut-jxl-sys's precedent: scan `$CARGO_HOME/registry/src/*/` for the pinned package, with
/// `JPEGLI_ORACLE_LIBJXL_DIR` as the override for vendored (`cargo vendor`) setups.
fn libjxl_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("JPEGLI_ORACLE_LIBJXL_DIR") {
        let dir = PathBuf::from(dir);
        assert!(
            dir.join("lib/jpegli").is_dir(),
            "JPEGLI_ORACLE_LIBJXL_DIR has no lib/jpegli: {}",
            dir.display()
        );
        return dir;
    }
    #[allow(deprecated)] // `home_dir` is un-deprecated since Rust 1.87; the lint name lags.
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))
        .expect("neither CARGO_HOME nor a home directory is available");
    let src_root = cargo_home.join("registry").join("src");
    let needle = format!("jpegxl-src-{JPEGXL_SRC_VERSION}");
    if let Ok(entries) = std::fs::read_dir(&src_root) {
        for index_dir in entries.flatten() {
            let candidate = index_dir.path().join(&needle).join("libjxl");
            if candidate.join("lib/jpegli").is_dir() {
                return candidate;
            }
        }
    }
    panic!(
        "could not locate the vendored libjxl of {needle} under {}; set JPEGLI_ORACLE_LIBJXL_DIR \
         to a libjxl v0.11 source tree",
        src_root.display()
    );
}

/// Reads a required build-time env var, panicking (this is a build script) if absent.
fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("missing build env var {key}"))
}

/// `Path` → `String`, panicking on non-UTF-8 paths (none exist in this build tree).
fn path_str(p: &Path) -> String {
    p.to_str()
        .unwrap_or_else(|| panic!("non-UTF-8 path: {}", p.display()))
        .to_string()
}
