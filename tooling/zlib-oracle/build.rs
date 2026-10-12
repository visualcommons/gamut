//! Builds a static `zlib` from the `third_party/zlib` submodule and generates FFI bindings for its
//! public API. Everything lands under `OUT_DIR`, so `cargo clean` fully resets the build and the
//! shipped gamut crates never need a C toolchain — this oracle is pulled in only as a dev-dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env("OUT_DIR"));
    let submodule = manifest_dir.join("../../third_party/zlib");
    assert!(
        submodule.join("CMakeLists.txt").exists(),
        "vendored zlib not found under {} — run `git submodule update --init --recursive`",
        submodule.display()
    );
    // zlib's CMake deletes the in-source `zconf.h` during an out-of-source build, which would dirty
    // the submodule. Build from a private copy under OUT_DIR so the vendored source stays pristine.
    let src = out_dir.join("zlib-src");
    if !src.join("CMakeLists.txt").exists() {
        copy_dir(&submodule, &src);
    }

    // ---- CMake-build a static zlib. -------------------------------------------------------
    let build = out_dir.join("zlib-build");
    if !build.join("CMakeCache.txt").exists() {
        run(Command::new("cmake")
            .arg("-S")
            .arg(&src)
            .arg("-B")
            .arg(&build)
            .args([
                "-DCMAKE_BUILD_TYPE=Release",
                // Rust links into a position-independent executable; the static archive must be
                // PIC or the final link fails with R_X86_64_32S relocation errors.
                "-DCMAKE_POSITION_INDEPENDENT_CODE=ON",
                // No examples/minigzip; we only need the static archive.
                "-DZLIB_BUILD_EXAMPLES=OFF",
            ]));
    }
    // `--parallel` takes an explicit count. Bare, it becomes `make -j` with no limit *and*
    // overrides CMAKE_BUILD_PARALLEL_LEVEL, so the shared dial would be silently ignored.
    run(Command::new("cmake").arg("--build").arg(&build).args([
        "--config",
        "Release",
        "--parallel",
        &build_env::build_parallelism().to_string(),
    ]));

    // ---- Link. zlib's CMake names the static archive `libz.a` (OUTPUT_NAME `z`) at the build
    // root on Unix; link it explicitly so the shared `libz.so` sitting beside it is ignored. ----
    println!("cargo:rustc-link-search=native={}", path_str(&build));
    println!("cargo:rustc-link-lib=static=z");

    // ---- Publish the headers of exactly this build to dependents (`DEP_Z_INCLUDE`, via
    // `links = "z"`), so a C/C++ consumer that links this archive also compiles against it rather
    // than against whatever `<zlib.h>` the system ships. Two directories, in search order: the
    // build tree (CMake-generated `zconf.h`) and the private source copy (`zlib.h`), joined with
    // the platform's path-list separator — split them with `std::env::split_paths`. ------------
    let include = std::env::join_paths([&build, &src]).expect("include dirs contain no separator");
    println!(
        "cargo:include={}",
        include
            .to_str()
            .unwrap_or_else(|| panic!("non-UTF-8 include path: {include:?}"))
    );

    // ---- Bindings. zlib.h lives in the source tree; the generated `zconf.h` in the build tree. --
    let bindings = bindgen::Builder::default()
        .header(path_str(&manifest_dir.join("wrapper.h")))
        .clang_arg(format!("-I{}", path_str(&build)))
        .clang_arg(format!("-I{}", path_str(&src)))
        .allowlist_function("inflate.*")
        .allowlist_function("deflate.*")
        .allowlist_function("compress.*")
        .allowlist_function("uncompress.*")
        .allowlist_function("adler32")
        .allowlist_function("crc32")
        .allowlist_type("z_stream.*")
        .allowlist_var("Z_.*")
        .allowlist_var("ZLIB_VERSION")
        .prepend_enum_name(false)
        .layout_tests(false)
        .generate()
        .expect("generate zlib FFI bindings");
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("write zlib bindings");

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed={}",
        path_str(&submodule.join("zlib.h"))
    );
}

/// Recursively copies `src` into `dst` (skipping `.git`) so an in-source-modifying build never
/// touches the vendored submodule.
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap_or_else(|e| panic!("create {}: {e}", dst.display()));
    for entry in std::fs::read_dir(src)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", src.display()))
        .flatten()
    {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        let target = dst.join(path.file_name().expect("dir entry has a name"));
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target)
                .unwrap_or_else(|e| panic!("copy {}: {e}", path.display()));
        }
    }
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

/// Runs a build subcommand, aborting the build with its output on failure.
fn run(cmd: &mut Command) {
    // Normalise any compiler-launcher env (sccache/ccache) into the single position cmake
    // defines for it, so this build is hermetic to exactly what it configures. See the
    // `build-env` crate docs; `GAMUT_BUILD_KEEP_ENV=1` opts out.
    build_env::sanitize(cmd);
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {cmd:?}: {e}"));
    assert!(status.success(), "command failed ({status}): {cmd:?}");
}
