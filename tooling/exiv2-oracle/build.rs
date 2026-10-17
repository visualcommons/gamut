//! Builds exiv2 0.28 (with its bundled Adobe XMP Toolkit, plus its PNG and BMFF image handlers)
//! and the expat it depends on, both from `third_party/` submodules, into `OUT_DIR`, then links
//! them behind the `extern "C"` shim in `src/shim.cpp`.
//!
//! expat is vendored and built static (rather than linked from the system) so the oracle is
//! hermetic; only the system `zlib` is needed (exiv2's PNG handler inflates/deflates `iCCP` and
//! `zTXt` through it), exactly as the other C oracles already require.
//! Everything lands under `OUT_DIR`, so `cargo clean` fully resets the build. CI needs the
//! submodules checked out (`git submodule update --init --recursive`) and a C++ toolchain.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env("OUT_DIR"));
    let third_party = manifest.join("../../third_party");
    let expat_src = third_party.join("expat/expat");
    let exiv2_src = third_party.join("exiv2");

    assert!(
        expat_src.join("CMakeLists.txt").exists(),
        "vendored expat not found under {} — run `git submodule update --init --recursive`",
        expat_src.display()
    );
    assert!(
        exiv2_src.join("CMakeLists.txt").exists(),
        "vendored exiv2 not found under {} — run `git submodule update --init --recursive`",
        exiv2_src.display()
    );

    // ---- expat: static, position-independent, no tools/tests. -----------------------------------
    let expat_install = out.join("expat-install");
    let expat_build = out.join("expat-build");
    if !expat_build.join("CMakeCache.txt").exists() {
        run(Command::new("cmake")
            .arg("-S")
            .arg(&expat_src)
            .arg("-B")
            .arg(&expat_build)
            .args([
                "-DCMAKE_BUILD_TYPE=Release",
                "-DBUILD_SHARED_LIBS=OFF",
                "-DCMAKE_POSITION_INDEPENDENT_CODE=ON",
                "-DEXPAT_SHARED_LIBS=OFF",
                "-DEXPAT_BUILD_TOOLS=OFF",
                "-DEXPAT_BUILD_EXAMPLES=OFF",
                "-DEXPAT_BUILD_TESTS=OFF",
                "-DEXPAT_BUILD_DOCS=OFF",
            ])
            .arg(format!(
                "-DCMAKE_INSTALL_PREFIX={}",
                expat_install.display()
            )));
    }
    // `--parallel` takes an explicit count. Bare, it becomes `make -j` with no limit *and*
    // overrides CMAKE_BUILD_PARALLEL_LEVEL, so the shared dial would be silently ignored.
    run(Command::new("cmake")
        .arg("--build")
        .arg(&expat_build)
        .args(["--config", "Release", "--target", "install", "--parallel"])
        .arg(build_env::build_parallelism().to_string()));

    // ---- exiv2: static, XMP (the bundled XMPCore) + PNG + BMFF; every other feature off. -------
    // The build and install dirs are keyed on the option set (`EXIV2_TAG`): configuring runs only
    // when `CMakeCache.txt` is absent, so a changed option set must land in a fresh directory
    // rather than silently reuse a cache configured with the old one. Change the tag whenever
    // the option list below changes.
    const EXIV2_TAG: &str = "png-bmff";
    let exiv2_install = out.join(format!("exiv2-install-{EXIV2_TAG}"));
    let exiv2_build = out.join(format!("exiv2-build-{EXIV2_TAG}"));
    if !exiv2_build.join("CMakeCache.txt").exists() {
        run(Command::new("cmake")
            .arg("-S")
            .arg(&exiv2_src)
            .arg("-B")
            .arg(&exiv2_build)
            .args([
                "-DCMAKE_BUILD_TYPE=Release",
                "-DBUILD_SHARED_LIBS=OFF",
                "-DCMAKE_POSITION_INDEPENDENT_CODE=ON",
                "-DEXIV2_ENABLE_XMP=ON", // pulls in the bundled Adobe XMPCore
                "-DEXIV2_ENABLE_PNG=ON", // PNG container handler (needs zlib)
                "-DEXIV2_ENABLE_NLS=OFF",
                "-DEXIV2_ENABLE_WEBREADY=OFF",
                "-DEXIV2_ENABLE_BMFF=ON", // ISOBMFF (AVIF/HEIF/JXL) handler, read-only in exiv2
                "-DEXIV2_ENABLE_BROTLI=OFF",
                "-DEXIV2_ENABLE_INIH=OFF",
                "-DEXIV2_ENABLE_VIDEO=OFF",
                "-DEXIV2_ENABLE_LENSDATA=OFF",
                "-DEXIV2_BUILD_SAMPLES=OFF",
                "-DEXIV2_BUILD_EXIV2_COMMAND=OFF",
                "-DEXIV2_BUILD_UNIT_TESTS=OFF",
                "-DEXIV2_BUILD_DOC=OFF",
            ])
            .arg(format!("-DCMAKE_PREFIX_PATH={}", expat_install.display()))
            .arg(format!(
                "-DCMAKE_INSTALL_PREFIX={}",
                exiv2_install.display()
            )));
    }
    run(Command::new("cmake")
        .arg("--build")
        .arg(&exiv2_build)
        .args(["--config", "Release", "--target", "install", "--parallel"])
        .arg(build_env::build_parallelism().to_string()));

    // ---- Compile the shim and link the static archives. ----------------------------------------
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .flag_if_supported("-w") // exiv2's public headers are warning-heavy reference code
        .include(exiv2_install.join("include"))
        .file(manifest.join("src/shim.cpp"))
        .compile("exiv2_shim");

    // CMake installs to lib or lib64 depending on the platform; search both.
    for base in [&exiv2_install, &expat_install] {
        println!(
            "cargo:rustc-link-search=native={}",
            base.join("lib").display()
        );
        println!(
            "cargo:rustc-link-search=native={}",
            base.join("lib64").display()
        );
    }
    // exiv2 references expat (XMPCore) and zlib; emit in dependency order.
    println!("cargo:rustc-link-lib=static=exiv2");
    println!("cargo:rustc-link-lib=static=expat");
    println!("cargo:rustc-link-lib=dylib=z");

    println!("cargo:rerun-if-changed=src/shim.cpp");
    println!("cargo:rerun-if-changed=build.rs");
}

/// Reads a required build-time env var, panicking (this is a build script) if absent.
fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("missing build env var {key}"))
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
