# c2pa-oracle

Dev-only differential oracle against [`c2pa-rs`](https://github.com/contentauth/c2pa-rs), the C2PA
reference implementation, for the container half of the C2PA epic (issue #239, this crate is issue
#447).

Not a member of the gamut workspace — it is listed under `[workspace].exclude` in the root
manifest and nothing depends on it, so `cargo build`/`test --workspace` never reaches it. Run it by
manifest path:

```bash
mise run test-c2pa    # the differential tests
mise run check-c2pa   # compile only, which is what the per-PR lint lane affords
```

## What it checks

The epic splits the work in two: **gamut locates, bounds, carries and reserves** a C2PA manifest
store; **validation belongs to `c2pa-rs`**. This crate is the seam between those halves, exercised
in both directions.

| direction | test file | what it pins |
| --- | --- | --- |
| gamut reserves → an external signer completes → c2pa-rs validates | `tests/reserve_then_fill.rs` | a store signed over the reserved file validates once patched into the range `encode_with_report` gave, and exactly fills it |
| c2pa-rs embeds → gamut locates the identical byte range | `tests/locate_embedded.rs` | `gamut-avif` and `gamut-heic` report the *same span* as the store's own JUMBF header, and c2pa-rs re-validates the bytes gamut extracted |
| the box itself | `tests/box_framing.rs` | `gamut-avif`'s `ContentProvenanceBox` is byte-identical to c2pa-rs's for the same store |
| a derivative carries no parent store | `tests/no_copy_forward.rs` | a re-encode reads back as **unsigned** (`JumbfNotFound`), not as invalid |
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

## Why gamut owns the locate/bound step at all

The obvious objection to `gamut-heic::HeifContainer::c2pa` and `gamut_avif::AvifContainer::c2pa` is
that they duplicate something `c2pa-rs` already does, and that a consumer who wants the store could
just call the reference implementation. That objection is wrong, and it is written down here so
nobody deletes gamut's locator as redundant later.

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
c2pa = { version = "0.90.21", default-features = false, features = ["rust_native_crypto"] }
```

`c2pa`'s default feature set is `["openssl", "default_http"]`. The `openssl` feature pulls OpenSSL
in **vendored**, compiling it from C source into a dev build — precisely the thing the epic's
no-crypto criterion exists to keep out, and it would make this oracle the slowest thing in CI.
`rust_native_crypto` is the pure-Rust signing and verification backend that replaces it; dropping
`default_http` additionally drops `reqwest` and `ureq`, which this oracle never needs because it
resolves no remote manifests.

`tests/build_configuration.rs` fails if that line ever loses either half. This crate is `c2pa`'s
only dependent in the repository, so nothing else can turn the feature back on by unification: the
manifest line is the whole determinant, which is what makes a drift guard over it sufficient.

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
