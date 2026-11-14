//! integration · drift guard — the umbrella's `metadata` feature forwards to the format crates.
//!
//! `gamut-jpeg`, `gamut-jxl` and `gamut-heic` each carry the typed metadata accessors behind their
//! own `metadata` Cargo feature. The umbrella's `metadata` feature forwards to all three weakly
//! (`gamut-jpeg?/metadata`), so that a consumer who asks the umbrella for a format *and* for
//! metadata gets the wiring between them, while neither half drags the other in on its own.
//!
//! **Why this cannot live in a lower crate.** `AGENTS.md` forbids pinning anything under
//! `crates/gamut/tests/` *by choice*, because `.cargo/mutants.toml` sets `test_workspace = false`,
//! so nothing here can kill a mutant in any crate but the umbrella — and the forwards this file
//! pins are `Cargo.toml` feature entries, which no mutant touches. This file is not here by
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
//!   format crate's `metadata` feature is on too. Observed by *resolution*: each name below exists
//!   only under that feature, so a dropped forward is a compile error.
//! - **Forward is weak, and belongs to `metadata`.** Enabling `metadata` alone must not pull a
//!   codec into a build that asked for none, and enabling a format alone must not pull the facade
//!   in. Both are properties of the feature *table* rather than of any compiled artefact — a
//!   single build cannot see either — so they are pinned as a drift guard over the manifest text.
//!
//! The complementary half of the negative direction — that a format feature alone pulls in no
//! facade crate — holds because each format crate's `metadata` feature is off by default, which is
//! that crate's property and is pinned in that crate. It is measured here only as evidence
//! (`cargo tree -p gamut --features "jpeg,jxl,heic"` lists no facade crate), not asserted.
//!
//! The rest of the umbrella's feature table is pinned here for the same linkage reason: the
//! container rule (each container primitive re-exported under the feature of its own name, and
//! pulled in by no other umbrella feature), `avif` enabling `gamut-avif` alone, and `jxl`'s weak
//! forward of `gamut-dng?/jxl-encode`. Each is an entry in `crates/gamut/Cargo.toml` or a `cfg` in
//! `crates/gamut/src/lib.rs`, and none is observable from a lower crate.

/// With `metadata` and all three format features on, every forwarded name resolves.
///
/// Nothing is called and no fixture is built: a fixture bug, a signature change or a parser defect
/// must not be able to fail this test. Two names per crate are enough to observe the edge *and* to
/// say which of the two possible faults broke it — a dropped forward, or a renamed accessor. They
/// fail in different combinations, so the compile error is a diagnosis rather than a puzzle.
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

    /// Accepts any *type* and does nothing, so a feature-gated re-export can be named without
    /// constructing a value of it.
    fn type_resolves<T>() {}

    // The feature witness. Each crate re-exports the facade's `Metadata` under its own
    // `#[cfg(feature = "metadata")]`, so these three paths resolve exactly when the forward fired,
    // and they name a type this branch does not own. A dropped forward breaks the witness *and*
    // the accessor beneath it; renaming an accessor breaks only the accessor. Without the witness
    // the two produce the same error, and the next reader cannot tell which happened.
    type_resolves::<gamut::jpeg::Metadata>();
    type_resolves::<gamut::jxl::Metadata>();
    type_resolves::<gamut::heic::Metadata>();

    // The accessors the forward exists to deliver: each crate's facade impl (`gamut-jpeg`'s
    // and `gamut-jxl`'s `MetadataSink` on their encoders, and the `TypedMetadata` that parses the
    // `RawMetadata` `gamut-heic`'s `HeifDecoder::metadata` returns), each reachable through a trait
    // re-export that is `#[cfg(feature = "metadata")]` inside its own crate, so the path resolves
    // only if the umbrella's forward switched that crate's `metadata` feature on. A rename here is
    // a signal to update the pin, not a false alarm.
    resolves(<gamut::jpeg::JpegEncoder as gamut::jpeg::MetadataSink>::with_metadata);
    resolves(<gamut::jxl::JxlEncoder as gamut::jxl::MetadataSink>::with_metadata);
    resolves(<gamut::heic::RawMetadata as gamut::heic::TypedMetadata>::blocks);
}

/// The entry list of the umbrella's own `metadata` feature, sliced out of `manifest`.
///
/// `None` when the feature's opening line or its closing bracket is not where this file reads
/// them, which the caller reports rather than asserting over text it did not find.
fn metadata_feature_entries(manifest: &str) -> Option<&str> {
    let after_open = manifest.split_once("\nmetadata = [")?.1;
    Some(after_open.split_once("\n]")?.0)
}

/// Every forward is weak and is listed under `metadata` alone.
///
/// `gamut-jpeg?/metadata` enables that crate's feature only if something else already brought the
/// crate in; `gamut-jpeg/metadata` — the same line without the `?` — would enable the optional
/// dependency itself, so asking the umbrella for metadata would silently compile three codecs.
/// A single build cannot observe the difference, so the feature table is read directly.
///
/// *Which* list an entry sits in matters as much as its form, and matching the whole file cannot
/// see that: moving `"gamut-jpeg?/metadata"` out of `metadata = [ … ]` and into `jpeg = [ … ]`
/// leaves the text present, while making `--features jpeg` alone resolve the format crate with its
/// metadata wiring — and therefore the entire facade — which is the build the weak form exists to
/// prevent. So presence is asserted over the feature's **own entry list**, and a manifest-wide
/// count pins that the entry is not *also* attached to a format feature.
///
/// This one carries no `cfg`, so under a feature set that compiles the resolution test above out —
/// `--features metadata` with no format — it is also what notices a forward being deleted.
#[test]
fn every_metadata_forward_is_weak_and_listed_under_metadata_alone() {
    // Compiled in, not read from disk: the pin travels with the crate, including in a package
    // built for publication.
    const MANIFEST: &str = include_str!("../Cargo.toml");

    let Some(entries) = metadata_feature_entries(MANIFEST) else {
        panic!(
            "the umbrella's `metadata = [ … ]` feature list is not where this pin reads it, so \
             none of the forwards below could be checked at all"
        )
    };

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
            entries.contains(&weak),
            "the umbrella's `metadata` feature no longer lists {weak}; a consumer enabling \
             `metadata` with that format can no longer reach its typed accessors — and if the \
             entry moved to that format's own feature, that format alone now drags in the whole \
             facade"
        );
        assert_eq!(
            MANIFEST.matches(weak.as_str()).count(),
            1,
            "{weak} is listed more than once; a second copy under a format feature makes that \
             format alone drag in the whole facade"
        );
    }
}

/// The umbrella's manifest, compiled in so the pins travel with the crate.
const MANIFEST: &str = include_str!("../Cargo.toml");

/// The `[features]` table of `manifest`, up to the next table header.
fn features_table(manifest: &str) -> Option<&str> {
    let after = manifest.split_once("\n[features]\n")?.1;
    Some(after.split_once("\n[").map_or(after, |(table, _)| table))
}

/// The text between `name = [` and its closing `]` in the `[features]` table, `None` when the
/// feature is not declared there. No entry in the table contains a `]`, so the first one closes it.
fn feature_entries<'a>(manifest: &'a str, name: &str) -> Option<&'a str> {
    let table = features_table(manifest)?;
    let after_open = table.split_once(&format!("\n{name} = ["))?.1;
    Some(after_open.split_once(']')?.0)
}

/// Each container primitive's feature enables its crate and nothing else, and no other feature
/// enables that crate — the one rule `gamut::{ifd,riff,isobmff,heif}` are re-exported under.
///
/// A second `dep:gamut-ifd` (as `metadata` used to carry) pulls the container into a build that
/// did not name it; the count is what notices one.
#[test]
fn each_container_feature_enables_its_crate_alone() {
    for container in ["ifd", "riff", "isobmff", "heif"] {
        let dep = format!("\"dep:gamut-{container}\"");
        assert_eq!(
            feature_entries(MANIFEST, container).map(str::trim),
            Some(dep.as_str()),
            "the `{container}` feature must be exactly [{dep}]"
        );
        assert_eq!(
            MANIFEST.matches(dep.as_str()).count(),
            1,
            "{dep} is listed by a feature other than `{container}`"
        );
    }
}

/// `avif` enables `gamut-avif` alone; it no longer compiles `gamut-av1` without exposing it.
#[test]
fn avif_enables_gamut_avif_alone() {
    assert_eq!(
        feature_entries(MANIFEST, "avif").map(str::trim),
        Some("\"dep:gamut-avif\""),
    );
}

/// `jxl` forwards gamut-dng's JPEG XL encode path weakly, and only from `jxl`.
///
/// Without the forward, `features = ["jxl", "dng"]` builds libjxl and still refuses
/// `Compression::JpegXl` on DNG encode; the strong form would compile gamut-dng under `jxl` alone.
#[test]
fn jxl_forwards_dng_jxl_encode_weakly() {
    const WEAK: &str = "\"gamut-dng?/jxl-encode\"";
    assert!(
        !MANIFEST.contains("\"gamut-dng/jxl-encode\""),
        "the forward is not weak"
    );
    assert!(
        feature_entries(MANIFEST, "jxl").is_some_and(|entries| entries.contains(WEAK)),
        "the `jxl` feature no longer lists {WEAK}"
    );
    assert_eq!(
        MANIFEST.matches(WEAK).count(),
        1,
        "{WEAK} is listed more than once"
    );
}

/// `all` lists every other feature the umbrella declares.
#[test]
fn all_lists_every_feature() {
    let Some(table) = features_table(MANIFEST) else {
        panic!("the umbrella's `[features]` table is not where this pin reads it")
    };
    let Some(all) = feature_entries(MANIFEST, "all") else {
        panic!("the umbrella declares no `all` feature where this pin reads it")
    };
    let declared = table
        .lines()
        .filter_map(|line| line.split_once(" = [").map(|(name, _)| name))
        .filter(|name| !name.starts_with(['#', ' ']) && !["default", "all"].contains(name));
    let mut seen = 0;
    for name in declared {
        seen += 1;
        assert!(
            all.contains(&format!("\"{name}\"")),
            "`all` does not list `{name}`"
        );
    }
    assert!(seen > 0, "no feature was read from the table");
}

/// With every container feature on, each container resolves under its own name.
#[cfg(all(
    feature = "ifd",
    feature = "riff",
    feature = "isobmff",
    feature = "heif"
))]
#[test]
fn each_container_feature_reexports_its_crate() {
    fn type_resolves<T>() {}

    type_resolves::<gamut::ifd::Ifd>();
    type_resolves::<gamut::riff::Chunk<'static>>();
    type_resolves::<gamut::isobmff::IsoBmffImage>();
    type_resolves::<gamut::heif::DecodedFrame>();
}

/// `gamut::tiff` and `gamut::dng` re-export gamut-ifd's types, not look-alikes: each is the very
/// type `gamut::ifd` names, so a value moves between the three paths without conversion.
#[cfg(all(feature = "ifd", feature = "tiff", feature = "dng"))]
#[test]
fn tiff_and_dng_reexport_the_ifd_types_themselves() {
    /// Compiles only when both arguments name one type.
    fn same_type<T>(_: core::marker::PhantomData<T>, _: core::marker::PhantomData<T>) {}
    use core::marker::PhantomData as P;

    same_type(P::<gamut::ifd::Ifd>, P::<gamut::tiff::Ifd>);
    same_type(P::<gamut::ifd::Ifd>, P::<gamut::dng::Ifd>);
    same_type(P::<gamut::ifd::Value>, P::<gamut::tiff::Value>);
    same_type(P::<gamut::ifd::Value>, P::<gamut::dng::Value>);
    same_type(P::<gamut::ifd::ByteOrder>, P::<gamut::tiff::ByteOrder>);
    same_type(P::<gamut::ifd::ByteOrder>, P::<gamut::dng::ByteOrder>);
}
