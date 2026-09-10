//! integration · drift guard — the umbrella's `metadata` feature forwards to the format crates.
//!
//! `gamut-jpeg`, `gamut-jxl` and `gamut-heic` each carry the typed metadata accessors behind their
//! own `metadata` Cargo feature. The umbrella's `metadata` feature forwards to all three weakly
//! (`gamut-jpeg?/metadata`), so that a consumer who asks the umbrella for a format *and* for
//! metadata gets the wiring between them, while neither half drags the other in on its own.
//!
//! **Why this cannot live in a lower crate.** `AGENTS.md` forbids pinning anything under
//! `crates/gamut/tests/` *by choice*, because `.cargo/mutants.toml` sets `test_workspace = false`
//! and excludes `crates/gamut/**`, so nothing here can kill a mutant. This file is not here by
//! choice: what it pins is an edge in the umbrella's own feature graph, and no lower crate can
//! observe that edge — `gamut-jpeg` cannot see who enabled its `metadata` feature, and the three
//! format crates must not gain dev-dependency edges on one another or on the umbrella
//! (`mise run check-release-deps`). That is the linkage exception the same rule names, and this
//! file is in its only legal home.
//!
//! **These tests are therefore mutation-invisible**, and nothing else holds the forwards: the
//! wiring they switch on is covered inside each format crate (`gamut-jpeg`'s, `gamut-jxl`'s and
//! `gamut-heic`'s own `metadata` suites, all mutation-visible), but the *forward* — the three
//! entries in `crates/gamut/Cargo.toml` — is pinned by this file alone. Delete it and a dropped
//! forward becomes silent again.
//!
//! The two directions are pinned by different techniques because only one of them is observable
//! from a compiled build:
//!
//! - **Forward fires.** With the umbrella's `metadata` feature and a format's feature both on, the
//!   format crate's `metadata` feature is on too. Observed by *resolution*: each accessor named
//!   below exists only under that feature, so a dropped forward is a compile error.
//! - **Forward is weak.** Enabling `metadata` alone must not pull a codec into a build that asked
//!   for none. That is exactly what the `?` in `gamut-jpeg?/metadata` means, and it is a property
//!   of the feature table rather than of any compiled artefact — a single build cannot see it — so
//!   it is pinned as a drift guard over the manifest text.
//!
//! The complementary half of the negative direction — that a format feature alone pulls in no
//! facade crate — holds because each format crate's `metadata` feature is off by default, which is
//! that crate's property and is pinned in that crate. It is measured here only as evidence
//! (`cargo tree -p gamut --features "jpeg,jxl,heic"` lists no facade crate), not asserted.

/// With `metadata` and all three format features on, every forwarded accessor resolves.
///
/// Nothing is called and no fixture is built: a fixture bug, a signature change or a parser defect
/// must not be able to fail this test. One item per crate is named — the fewest it takes to
/// observe the three edges — so the only ways this can break are the forward being dropped and the
/// item being renamed, and a rename is a signal to update the pin rather than a false alarm.
#[cfg(all(
    feature = "metadata",
    feature = "jpeg",
    feature = "jxl",
    feature = "heic"
))]
#[test]
fn the_metadata_feature_reaches_each_format_crates_accessors() {
    /// Accepts any item and does nothing: naming one as the argument is the whole assertion, and
    /// the generic parameter keeps the pin independent of the item's signature.
    fn resolves<T>(_item: T) {}

    // Each is `#[cfg(feature = "metadata")]` inside its own crate, so the path resolves only if
    // the umbrella's forward switched that crate's `metadata` feature on.
    resolves(gamut::jpeg::JpegMetadata::blocks);
    resolves(gamut::jxl::JxlMetadata::blocks);
    resolves(gamut::heic::HeifImage::blocks);
}

/// Every forward is weak, so `metadata` alone pulls no codec into the build.
///
/// `gamut-jpeg?/metadata` enables that crate's feature only if something else already brought the
/// crate in; `gamut-jpeg/metadata` — the same line without the `?` — would enable the optional
/// dependency itself, so asking the umbrella for metadata would silently compile three codecs.
/// A single build cannot observe the difference, so the feature table is read directly.
///
/// This one carries no `cfg`, so under a feature set that compiles the resolution test above out —
/// `--features metadata` with no format — it is also what notices a forward being deleted.
#[test]
fn every_format_metadata_forward_is_weak() {
    // Compiled in, not read from disk: the pin travels with the crate, including in a package
    // built for publication.
    const MANIFEST: &str = include_str!("../Cargo.toml");

    for crate_name in ["gamut-jpeg", "gamut-jxl", "gamut-heic"] {
        let weak = format!("\"{crate_name}?/metadata\"");
        let strong = format!("\"{crate_name}/metadata\"");
        // The strong form is checked first so that dropping the `?` is diagnosed as dropping the
        // `?`. Checking presence first would report a de-weakened forward as a missing one, which
        // sends the next reader after the wrong fault.
        assert!(
            !MANIFEST.contains(&strong),
            "the forward {strong} is not weak; enabling `metadata` alone would now pull \
             {crate_name} into builds that asked for no codec"
        );
        assert!(
            MANIFEST.contains(&weak),
            "the umbrella's `metadata` feature no longer forwards {weak}; a consumer enabling \
             `metadata` with that format can no longer reach its typed accessors"
        );
    }
}
