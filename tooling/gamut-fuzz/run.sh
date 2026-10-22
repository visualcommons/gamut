#!/usr/bin/env bash
# Runner for the dev-only fuzz tier (issues #264, #311), invoked as `mise run fuzz [target] [args]`.
#
# A script rather than an inline `run =` because mise appends a task's arguments to the end of its
# command, which a multi-line shell body cannot absorb -- the same reason `tooling/mutants/run.sh`
# exists.
set -euo pipefail

FUZZ_DIR="tooling/gamut-fuzz"

# cargo-fuzz 0.13 defaults `--target` to x86_64-unknown-linux-musl, and the sanitizer cannot link
# against a static libc ("sanitizer is incompatible with statically linked libc"). Pinning the host
# gnu triple is what makes the build reach a fuzz target at all.
target="$(rustc -vV | sed -n 's/^host: //p')"

if [ "$#" -eq 0 ]; then
	echo "fuzz targets (run one with: mise run fuzz <target>):"
	cargo +nightly fuzz list --fuzz-dir "$FUZZ_DIR"
	echo
	echo "  mise run fuzz ifd_read_ledger                        # until it finds something, or Ctrl-C"
	echo "  mise run fuzz ifd_read_ledger -- -max_total_time=60  # bounded"
	echo
	echo "A crash is minimised (cargo +nightly fuzz tmin --fuzz-dir $FUZZ_DIR <target> <input>)"
	echo "and then promoted into a NAMED DETERMINISTIC TEST in the crate's own suite."
	echo "The corpus is a search aid, not the regression record."
	echo
	echo "  mise run fuzz cmin <target> <corpus-dir>              # minimise a corpus in place"
	exit 0
fi

# `mise run fuzz cmin <target> <dir>` minimises a corpus directory in place (#603): the Extended job
# runs it on the persisted search corpus before caching it, so the cache holds the inputs that add
# coverage rather than everything the engine ever wrote. A subcommand of this runner rather than a
# bare `cargo fuzz cmin` in the workflow so it carries the same host-triple pin as `run` below and a
# contributor reproduces exactly what CI does. No target is named `cmin`; `check-targets.sh` keeps
# the target set to the files under `fuzz_targets/`.
if [ "$1" = "cmin" ]; then
	shift
	exec cargo +nightly fuzz cmin --fuzz-dir "$FUZZ_DIR" --target "$target" "$@"
fi

# ── Reconstructing the `--` cargo-fuzz needs ────────────────────────────────────────────────
#
# `cargo fuzz run TARGET [CORPUS...] [-- <libFuzzer args>]` takes libFuzzer's arguments only after
# a `--`. mise SWALLOWS that separator: `mise run fuzz ifd_read_ledger -- -max_total_time=600`
# arrives here as two words, `ifd_read_ledger` and `-max_total_time=600`, and cargo-fuzz then
# rejects the second as one of its own options --
#
#     error: Found argument '-m' which wasn't expected, or isn't valid in this context
#
# which is how the CI job failed on run 33467776925. `tooling/mutants/run.sh` documents the same
# swallow; this runner did not guard for it, so the bounded form the README advertises had never
# actually worked. The separator is therefore reconstructed rather than relied on.
#
# The discriminator is libFuzzer's own flag syntax, which is always `-name=value` -- a single dash
# and an `=`. cargo-fuzz's options are `--long`, `--long=value` or a single-letter short, none of
# which match. So `--release` and `-s none` still reach cargo-fuzz, and `-runs=100` still reaches
# libFuzzer, with or without a `--`. An explicit `--` (invoking this script directly, where nothing
# eats it) is honoured too: everything after it is libFuzzer's, unconditionally.
cargo_args=()
libfuzzer_args=()
seen_separator=""
for arg in "$@"; do
	if [ -n "$seen_separator" ]; then
		libfuzzer_args+=("$arg")
	elif [ "$arg" = "--" ]; then
		seen_separator=1
	elif [[ $arg == -[!-]*=* ]]; then
		libfuzzer_args+=("$arg")
	else
		cargo_args+=("$arg")
	fi
done

# ── Dictionaries (#567) ─────────────────────────────────────────────────────────────────────
#
# The TIFF/IFD parser targets run with a libFuzzer `-dict` of the tokens real-camera files carry
# (tag IDs in both byte orders, MakerNote magics, DNG opcode IDs) -- committed under `dict/` in
# place of a fetched real-camera seed corpus; README "Dictionaries" says why. Applied here rather
# than in the workflow so `mise run fuzz <target>` locally searches exactly as the Extended job
# does. libFuzzer reads a single `-dict` file, so a target that needs several gets them
# concatenated into the ignored `state/` directory. An explicit `-dict=` on the command line wins.
case "${cargo_args[0]:-}" in
ifd_read | tiff_decode) dicts=(ifd) ;;
dng_decode) dicts=(ifd dng) ;;
*) dicts=() ;;
esac
user_dict=""
for arg in ${libfuzzer_args[@]+"${libfuzzer_args[@]}"}; do
	if [[ $arg == -dict=* ]]; then
		user_dict=1
	fi
done
if [ ${#dicts[@]} -gt 0 ] && [ -z "$user_dict" ]; then
	merged="$PWD/$FUZZ_DIR/state/${cargo_args[0]}.dict"
	mkdir -p "$PWD/$FUZZ_DIR/state"
	: >"$merged"
	for name in "${dicts[@]}"; do
		cat "$FUZZ_DIR/dict/$name.dict" >>"$merged"
	done
	libfuzzer_args+=("-dict=$merged")
fi

invocation=(run --fuzz-dir "$FUZZ_DIR" --target "$target")
if [ ${#cargo_args[@]} -gt 0 ]; then
	invocation+=("${cargo_args[@]}")
fi
if [ ${#libfuzzer_args[@]} -gt 0 ]; then
	invocation+=(-- "${libfuzzer_args[@]}")
fi

exec cargo +nightly fuzz "${invocation[@]}"
