//! The one thing about this crate that is not a differential: the `c2pa` dependency must never
//! regain its default features.
//!
//! `c2pa`'s defaults are `["openssl", "default_http"]`, and `openssl` is pulled **vendored** —
//! enabling it would compile OpenSSL from C source into a dev build of this repository. The #239
//! epic's "no crypto in the shipped graph" criterion is about the *shipped* crates, but the
//! vendored build is also simply the slowest thing that could be added to CI, and a switch back
//! would be silent: everything would still pass, just far more slowly and with a C toolchain
//! newly on the critical path.
//!
//! Two checks, weakest sufficient technique each. The manifest check is a drift guard on the line
//! a human would edit; the lockfile check is a **resolved-graph** assertion and is the stronger of
//! the two, because it would also catch the feature arriving by unification from somewhere else.

use std::path::Path;

/// The crate's own manifest, read at compile time so the test cannot silently pass against a
/// different file.
const MANIFEST: &str = include_str!("../Cargo.toml");

/// Package names that only appear in the resolved graph when the `openssl` feature is on.
const OPENSSL_PACKAGES: [&str; 3] = ["openssl", "openssl-sys", "openssl-src"];

#[test]
fn the_c2pa_dependency_line_disables_default_features_and_asks_only_for_rust_native_crypto() {
    let line = MANIFEST
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("c2pa = "))
        .expect("the manifest declares a `c2pa` dependency on one line");

    assert!(
        line.contains("default-features = false"),
        "the `c2pa` dependency must disable default features (they are [\"openssl\", \
         \"default_http\"], and `openssl` is vendored): {line}"
    );
    assert!(
        line.contains(r#"features = ["rust_native_crypto"]"#),
        "the `c2pa` dependency must ask for exactly the pure-Rust crypto backend: {line}"
    );
    assert!(
        !line.contains("openssl"),
        "the `c2pa` dependency must never name the `openssl` feature: {line}"
    );
}

#[test]
fn the_resolved_dependency_graph_contains_no_openssl_package() {
    // Cargo writes this before it builds the test, so it always exists by the time the test runs;
    // it is `.gitignore`d because a `tooling/` oracle resolves standalone.
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock)
        .unwrap_or_else(|error| panic!("reading {}: {error}", lock.display()));

    for package in OPENSSL_PACKAGES {
        assert!(
            !lock.contains(&format!("name = \"{package}\"")),
            "`{package}` reached the resolved graph, so the `c2pa` dependency has regained its \
             default `openssl` feature and this oracle now compiles OpenSSL from C source"
        );
    }
}
