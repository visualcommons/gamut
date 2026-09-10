# c2pa-oracle

Dev-only differential oracle against [`c2pa-rs`](https://github.com/contentauth/c2pa-rs), the C2PA
reference implementation, for the container half of the C2PA epic (issue #239, this crate is issue
#447).

Not a member of the gamut workspace — it is listed under `[workspace].exclude` in the root
manifest and nothing depends on it, so `cargo build`/`test --workspace` never reaches it. Run it by
manifest path:

```bash
mise run test-c2pa    # the differential tests
mise run check-c2pa   # compile only, in seconds, without signing anything

# and, when a claim below needs re-deriving rather than re-checking:
cargo run --manifest-path tooling/c2pa-oracle/Cargo.toml --example probe
```

**Nothing in CI runs either of those two tasks.** No workflow under `.github/` names this crate,
and being workspace-`exclude`d with no dependents it is invisible to `cargo clippy --workspace` and
`cargo test --workspace` as well. Its entire automated reach today is `fmt-tooling-check`, which
`mise run fmt-check` hangs off — formatting, and nothing else. Wiring `check-c2pa` into the per-PR
lint lane and `test-c2pa` into extended, mirroring `check-dng-real`/`test-dng-real`, is
[issue #541](https://github.com/visualcommons/gamut/issues/541); until it lands, run them by hand
after touching `gamut-avif` or `gamut-heic`.

`examples/probe.rs` asserts nothing. It prints what c2pa-rs actually does with a gamut-written
AVIF — the composed box's framing bytes, both directions' offsets and lengths, what each locator
reports, and the layout of an update-manifest pair — so the findings recorded below can be checked
against a newer `c2pa-rs` without reading its source.

## What it checks

The epic splits the work in two: **gamut locates, bounds, carries and reserves** a C2PA manifest
store; **validation belongs to `c2pa-rs`**. This crate is the seam between those halves, exercised
in both directions.

| direction | test file | what it pins |
| --- | --- | --- |
| gamut reserves → an external signer completes → c2pa-rs validates | `tests/reserve_then_fill.rs` | a store signed over the reserved file validates once patched into the range `encode_with_report` gave, and exactly fills it |
| c2pa-rs embeds → gamut locates the identical byte range | `tests/locate_embedded.rs` | `gamut-avif` and `gamut-heic` report the *same span* as the store's own JUMBF header, and c2pa-rs re-validates the bytes gamut extracted |
| the box itself | `tests/box_framing.rs` | `gamut-avif`'s `ContentProvenanceBox` is byte-identical to c2pa-rs's for the same store |
| a derivative carries no parent store | `tests/no_copy_forward.rs` | the parent's located store, carried through `gamut-metadata`'s `C2paPolicy` and into a re-encode, reads back as **unsigned** (`JumbfNotFound`) |
| the build stays crypto-free where it must | `tests/build_configuration.rs` | the `c2pa` dependency never regains its default `openssl` feature, in the manifest and in the resolved graph |
| the one BMFF layout gamut cannot discriminate | `tests/update_manifest.rs` | what c2pa-rs actually emits for `box_purpose = update` — see below |

## The `update`-purpose finding

`crates/gamut-heic/STATUS.md` carries a deferred row. C2PA 2.4 §A.5.3 states the framing for
`box_purpose` `manifest` and `original` — an 8-byte merkle offset, then the store — and for
`update` says nothing at all about the bytes ahead of the store. gamut therefore *probes*: offset 8
first, offset 0 as a fallback. A store written without the offset under `update` is the one in-spec
layout that probe cannot discriminate, and the row records it as mis-bounded rather than rejected,
on the assumption that "no known writer emits either shape".

`tests/update_manifest.rs` turns that assumption into an observation. Driven through
`BuilderIntent::Update` — c2pa-rs exposes no way to set the purpose string directly — the reference
implementation:

- **writes the 8-byte merkle offset in front of an `update` store exactly as it does for the other
  two purposes** (its `write_c2pa_box` takes the same branch for every purpose except `merkle`);
- relabels the file's earlier store `original`, as §A.5.3 requires;
- accepts the resulting two-store file as `Valid`.

So the ambiguous layout is not one the reference implementation emits, the probe's offset-8 arm is
the one that fires on real files, and its offset-0 fallback is dead weight against everything in
circulation rather than a source of mis-bounding. `tests/locate_embedded.rs` adds the other half:
every store c2pa-rs writes opens `LBox` + `jumb`, which is the `TBox` check that would make the
bound self-checking. Neither observation makes the `LBox` bound self-checking on its own — that is
issue #505 — but together they replace an assumption with evidence.

## The no-padding finding

`gamut-avif` bounds a slot by the **box** that carries it; `gamut-heic` bounds a store by its own
JUMBF **`LBox`**. Those are different bounds, and `tests/locate_embedded.rs` asks both locators for
the same file and compares each against the same independently derived span. They agree — and the
reason they can is a second observation about the reference implementation, recorded here for the
same reason the `update`-purpose one is:

> **c2pa-rs sizes the `ContentProvenanceBox` to the store exactly. It writes no padding between the
> end of the JUMBF superbox and the end of the box that carries it.**

Nothing in C2PA 2.4 §A.5.1.2 requires that. A writer that reserved a slot larger than the store it
finally signed — which is exactly what `gamut-avif`'s own `with_c2pa_reserved` seam permits, and
what `C2paSlot::slot_bytes` documents as "the store, then any padding" — would produce a file where
the box-bounded and `LBox`-bounded answers legitimately differ.

So the two claims are asserted separately. `gamut_avif_bounds_the_store_c2pa_rs_embedded_by_the_box_that_carries_it`
holds gamut only to *containment*: the slot begins where the store begins and holds every byte of
it. `c2pa_rs_leaves_no_padding_between_the_store_and_the_end_of_its_box` asserts the equality on its
own, and is named for c2pa-rs because c2pa-rs is what it measures. A future release that started
padding would fail that one test, and the diagnosis would be in its name rather than in a locator
test wrongly accusing gamut of mis-bounding.

## What this oracle does not check

Three limits, stated so nobody reads a green run as covering them.

**Where the box sits.** C2PA 2.4 §A.5.3 constrains a `ContentProvenanceBox`'s *placement* among the
top-level boxes. c2pa-rs validates a store wherever it finds one, so no assertion here can
distinguish a conforming placement from a non-conforming one, and none tries: an oracle that cannot
see a property must not be read as having checked it. Placement is `gamut-avif`'s own suite's
business — `crates/gamut-avif/tests/c2pa.rs` — where the writer's byte layout is the subject.
`AvifContainer::c2pa_slots` deliberately reports a store outside that window as found,
with its true range, rather than rejecting it, so the container stays a lens over bytes.

**Whose key signed the file.** `ValidationState::Trusted` is unreachable here *by construction* —
not merely unasserted — because the signing identity is ephemeral and on no trust list; see
[The signing identity](#the-signing-identity) below. Reaching it would need a trust list and a
committed certificate with a real expiry, a different subject belonging to a later slice of the
#239 epic. Nothing of the sort is checked into this tree, and no assertion here asks for it.

**That a store ends inside its enclosing box.** `jumbf_superbox_span` takes the store's declared
length at its word and checks it only against the end of the *buffer*, never against the end of the
`ContentProvenanceBox` around it. A store whose `LBox` overruns its own box but still fits the file
is reported as a span. Bounding it properly would mean parsing ISOBMFF framing here — which would
make this oracle depend on exactly the structural understanding it exists to check independently,
and a second copy of gamut's §A.5.1.2 walk proves nothing about the first. The independence is
worth more than the extra check, so the limit is accepted rather than closed. It costs nothing on
today's fixtures, whose stores run to the end of their box; issue #534, which points this crate at
PNG, TIFF and RIFF, is where a store followed by more container bytes first appears.

## Why gamut owns the locate/bound step at all

The obvious objection to `gamut-heic::HeifContainer::c2pa` and
`gamut_avif::AvifContainer::c2pa_slot` is that they duplicate something `c2pa-rs` already does, and
that a consumer who wants the store could just call the reference implementation. That objection is
wrong, and it is written down here so nobody deletes gamut's locator as redundant later.

**`c2pa-rs` has no cheap parse-only mode.** Its reading entry point is `Reader`, which validates:
it parses the store, checks the hard binding against the asset, verifies the COSE signature and
consults a trust list. There is no "tell me where the bytes are and do not judge them" path. The
settings can *disable* verification — and that is exactly the trap, because
`ValidationState::Invalid` is documented as covering both outcomes:

> The manifest store fails to meet `ValidationState::WellFormed` requirements, meaning it cannot
> even be parsed or its basic structure is non-compliant.
>
> **This case may also occur if validation is disabled in the SDK.**
>
> — `c2pa` 0.90.21, `src/validation_results.rs:36-41`

So a caller who turns validation off to use `c2pa-rs` as a locator gets back a verdict
indistinguishable from "this file is broken". A container library cannot build on that. Worse, the
answer a *container* needs — the store's byte range, for byte accounting, for extraction, and for
patching a reserved slot — is not something `Reader` returns at all.

Two further consequences follow, and both are load-bearing:

- **gamut must not depend on `c2pa-rs`.** Reaching for it would drag COSE, X.509 and RSA/ECDSA into
  the shipped dependency graph of an image library, which the epic forbids outright ("no crypto in
  the shipped graph"). It would also break `mise run check-cross wasm32-unknown-unknown`.
- **gamut must never report a validity verdict.** Its types are named for what they are — a
  `C2paSlot`, a `C2paManifestStore`, a byte `Range` — and document that the range is
  *observability*, not a hash exclusion range.

`c2pa-rs` is therefore the right tool for exactly one job, validation, and it does that job here,
in `tooling/`, where a dev-dependency tree costs a shipped consumer nothing.

## Build configuration is not optional

```toml
c2pa = { version = "=0.90.21", default-features = false, features = ["rust_native_crypto"] }
```

`c2pa`'s default feature set is `["openssl", "default_http"]`, and its own manifest declares

```toml
openssl = { version = "0.10.80", features = ["vendored"], optional = true }
```

— so the default build compiles OpenSSL from C source into a dev build of this repository. That is
precisely the thing the epic's no-crypto criterion exists to keep out, and it would make this
oracle the slowest thing in CI.
`rust_native_crypto` is the pure-Rust signing and verification backend that replaces it; dropping
`default_http` additionally drops `reqwest` and `ureq`, which this oracle never needs because it
resolves no remote manifests.

`tests/build_configuration.rs` fails if that line ever loses either half. This crate is `c2pa`'s
only dependent in the repository, so nothing else can turn the feature back on by unification: the
manifest line is the whole determinant, which is what makes a drift guard over it sufficient.

The version is pinned with `=`, not a caret range, and that is a separate guarantee from the
features. This file cites c2pa-rs's own source **by line number** — `src/validation_results.rs:36-41`
below, `src/jumbf_io.rs:246` and `:258` at the end — and a caret range lets any patch release move
those lines while the citation stands as written. `tests/build_configuration.rs` guards the pin, so
raising the version is a deliberate act that also means re-reading every citation here.

`Cargo.lock` is committed as well, for this crate alone. `.gitignore` excludes
`tooling/*/Cargo.lock` — a `tooling/` crate normally resolves through the root lockfile, so a local
one is redundant — and this crate is the one exception, with the negation and its reason recorded
beside the rule. The reason is that the `=` pin holds exactly one line: everything *under* `c2pa`,
307 transitive packages, re-resolves on every invocation without a lockfile. The no-OpenSSL check
above is an assertion about the resolved graph, and with the graph re-resolved moments earlier it
could only ever inspect what cargo had just written for it. Committing the lockfile is what makes
it a check; the exception is itself pinned by a drift guard, so it cannot be reverted in one line
without a test going red.

307 is the version-aware closure of `c2pa`'s entries in the committed lockfile, `c2pa` itself
excluded: the packages whose versions the `=` pin does **not** hold, which is the population this
argument is about — an `openssl` package can only arrive beneath `c2pa`, and it is *resolution*, not
compilation, that a missing lockfile redoes. Re-derive it from the lockfile:

```bash
python3 - <<'PY'
import tomllib

package = tomllib.load(open("tooling/c2pa-oracle/Cargo.lock", "rb"))["package"]
by_key = {(p["name"], p["version"]): p for p in package}
versions = {}
for p in package:
    versions.setdefault(p["name"], []).append(p["version"])

def key(dependency):
    field = dependency.split()
    return (field[0], field[1] if len(field) > 1 else versions[field[0]][0])

seen, todo = set(), [("c2pa", "0.90.21")]
while todo:
    node = todo.pop()
    if node in seen:
        continue
    seen.add(node)
    todo += [key(d) for d in by_key[node].get("dependencies", [])]

print(len(seen) - 1)
PY
```

The version has to be carried through the walk: 21 package names occur at two versions in this
lockfile, so a name-keyed closure is not the same set. The whole file holds 326 entries
(`grep -c '^name = ' tooling/c2pa-oracle/Cargo.lock`) — those 307, `c2pa` itself, this crate, and 17
reachable only through its four `gamut-*` dev-dependencies, which the root workspace lockfile
resolves. Fewer are ever compiled: 240 with default features off
(`cargo tree --manifest-path tooling/c2pa-oracle/Cargo.toml --locked -e normal -p c2pa
--prefix none | sed 's/ (\*)$//' | sort -u | wc -l`, `c2pa` included), because a lockfile pins
optional dependencies this build never turns on. That smaller figure is not the one the argument
needs: re-resolution moves all 307.

Committing it is only half. Cargo regenerates a missing or stale lockfile without complaint, so
`mise run test-c2pa` and `mise run check-c2pa` pass **`--locked`**: the resolution in the tree is
the resolution that was used, or the task fails. That is *not* the same claim as "nothing else in
the tree depends on `c2pa`" — true, but it bears on the feature line, saying no other dependent can
unify `openssl` back on, and says nothing about which versions those 307 packages resolve to.

## The signing identity

`c2pa::EphemeralSigner` mints a self-signed CA and an end-entity certificate in memory, Ed25519,
with the key usage and EKU the C2PA certificate profile wants. No key material is committed to this
tree and no fixture has to be rotated when it expires, because it never persists.

An ephemeral certificate is on no trust list, so `verify.verify_trust` is turned off in
`signing_context()` and every assertion is written against `ValidationState::Valid` — well-formed,
hard binding intact, signature verified. `ValidationState::Trusted` is unreachable here by
construction and nothing asks for it. That is the right target: this oracle measures whether gamut
moved a byte it should not have, not whose key signed the file.

## Not built on: `c2pa::jumbf_io`

The module is `pub` and looks like exactly the low-level seam this crate wants. It is not used, for
two reasons.

Half of it is not usable at all. `load_jumbf_from_stream` and `save_jumbf_to_stream`
(`src/jumbf_io.rs:246`, `:258`) take `&mut dyn CAIRead` / `&mut dyn CAIReadWrite`, and `asset_io` —
the module those traits live in — is crate-private. An external caller cannot spell the argument
types. That is a private-in-public leak, not an API.

The other half (`load_jumbf_from_memory`, `save_jumbf_to_memory`) *is* spellable, and is
deliberately still not used. Those functions return the store's **bytes**, never its offsets, so
they cannot answer the question a container library asks; and building on an undocumented
lower-level entry point would tie this oracle to internals that carry no stability promise, when
`Builder` and `Reader` express everything the two directions need. Where the oracle needs an
independent view of where a store sits, it derives it from the store's own JUMBF header — see
`find_jumbf_superbox` in `src/lib.rs`.

## Reading a JUMBF header, on the reference side

That independent view is the one thing this crate parses itself, so it has to be right on inputs
c2pa-rs never produces: a wrong span here is an oracle handing gamut a wrong answer and calling it
the reference one.

A JUMBF box is a JPEG-family *standard box* — `LBox` (4 bytes, big-endian), `TBox` (4 bytes), then
optionally `XLBox`. C2PA 2.4 §8.4.2.3 is the only place the vendored specification writes any of
that down, and it writes down only part: defining the C2PA salt, it calls it "a standard box
consisting of: a box length (LBox, as a 4-byte big-endian unsigned integer); a box type (TBox,
4-byte big-endian unsigned integer …)". That is the whole of it — `LBox` occurs exactly once in
`references/c2pa/C2PA_Specification_2.4.html` and `XLBox` not at all — so the vendored document
never mentions the oversized-header field and states no reserved `LBox` value. The full grammar,
*including* the two reserved values, is ISO 19566-5:2023, which is paywalled and not vendored here
(its procurement is issue #441). The two arms below are therefore the convention as it is
universally implemented, read against `c2pa-rs`'s behaviour — not a clause this crate can cite:

- **`LBox == 0`** — the box runs to the end of the file, so its length is however much of the
  buffer follows its own first byte;
- **`LBox == 1`** — the length is the 8-byte big-endian `XLBox` after `TBox`, counting the whole
  box including that 16-byte header.

`declared_store_len` implements both, and applies one rule to *every* arm — the rule
`gamut_isobmff`'s box reader already applies, that a length counting a header can never be less
than that header: `LBox` in 2..=7 against the 8-byte header, `XLBox` below 16 against the 16-byte
one, and `LBox == 0` in a buffer of fewer than 8 bytes, whose to-end-of-buffer length counts that
same 8-byte header. Any of them would otherwise yield a span ending at or before the store's first
body byte. All three refusals are the typed `OracleError::UnusableSuperboxLength`, and no length is
ever guessed. No store this crate has seen uses either reserved value — c2pa-rs writes a plain
32-bit `LBox` — which is precisely why the handling is written rather than assumed, and why the
arms are pinned by unit tests in `src/lib.rs` rather than left to a fixture that cannot reach them.

Each side of each refusal is pinned by its own test — the input the branch refuses, and the nearest
input it must *not* refuse — so widening or narrowing a range by one is caught. That is a claim
about a *set* of branches, and three consecutive reviews each found one more member of the set
untested, so the set is now written down rather than argued: the module documentation on
`#[cfg(test)] mod tests` in `src/lib.rs` carries the enumeration as a table, one row per
discriminating branch in four **named** functions — `find_jumbf_superbox`, `declared_store_len`,
`jumbf_superbox_span` and `is_jumbf_not_found` — naming the test on each side. It is a list, not a
rule membership can be derived from: a table that claimed to cover "every branch that chooses
between two answers about a buffer" would exclude the `is_jumbf_not_found` row, which chooses
between two answers about an *error*. The two discriminating branches deliberately left out, and
why, are named in that same doc comment.

Three tests in that module parse the table out of the doc comment and fail on a blank cell, or on a
name nothing in the file defines — rustdoc never compiles a `cfg(test)` module and never resolves
these links, so a renamed test would otherwise leave a stale row behind a green suite. That is the
whole of what they enforce. They read **rows → tests**: a branch added to one of those four
functions *without* a row leaves all of them green, as does a test added without one, so the
table's completeness is held by review and not by the suite. The per-function branch-count guard
that would close that direction is
[issue #616](https://github.com/visualcommons/gamut/issues/616).

Every one of those tests asserts **which** refusal fired — the message where the variant carries
one, the variant itself where it does not — never merely that an error occurred. Several branches
refuse the same input for different reasons: delete the truncated-`LBox` arm and a three-byte
buffer falls into the `LBox == 0` arm, which refuses it too, so only the message distinguishes the
branch that fired from the one that caught the fall.

For the same reason `find_jumbf_superbox` **continues** past a `jumb` that appears too early to
carry an `LBox` in front of it, instead of concluding the buffer has no superbox. Today's fixtures
put a fixed ISOBMFF framing ahead of the store, so nothing can trip it; issue #534 points this
crate at PNG, TIFF and RIFF, where a store follows arbitrary compressed bytes.
