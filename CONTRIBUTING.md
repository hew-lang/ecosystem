# Contributing

## Before you push

```sh
make verify            # toolchain pins and the package metadata contract
```

```sh
docker compose up -d               # the services the suites and examples use
scripts/corpus-gate.sh all         # the corpus gate, the same script CI runs
cargo test --locked --workspace    # the native crates
```

`scripts/corpus-gate.sh` type-checks every tracked `.hew` source and then runs
every suite and every example. CI invokes that same script, so there is no
second list to keep in step with it. [`docs/toolchain.md`](docs/toolchain.md)
describes its stages and the system dependencies each one needs.

Everything must be warning-free under the pinned compiler, not just
error-free. `hew fmt --check` and `cargo fmt --check` must pass; a `lefthook`
pre-commit hook formats staged files for you if you have it installed.

## Adding a package

1. Create the directory matching the dotted name (`hew.queue.kafka` →
   `queue/kafka`) with a `hew.toml`, a module named after the final segment
   (`kafka.hew`), a `README.md`, `examples/`, and `tests/`.
2. If the top-level segment is new, add the mirror symlink under `hew/`.
3. If the package wraps a Rust library, add the crate to the workspace
   `Cargo.toml` and follow the native-crate contract in
   [`docs/packaging.md`](docs/packaging.md).
4. Bump the expected counts in `scripts/verify_package_contract.py` —
   `EXPECTED_MANIFESTS` for the new package, `EXPECTED_SOURCES` for every
   `.hew` file it adds. Both counts are written there and nowhere else;
   `scripts/corpus-gate.sh` reads the source count from that script rather
   than restating it.

## What a package looks like

The packages here are deliberately small — the point is that you can read one
in a sitting. Keep the public surface narrow, return `Result<T, E>` with a
package-specific error enum rather than sentinel values, give that enum an
`error_message`, and put the failure in the type rather than in a comment. Own
external resources in an actor so lifetime is explicit. Match variants in
dotted form (`.Ok`, `.Err`).

Each README opens with one runnable example, and `examples/` holds the same
program as a file. The gate runs them, so they cannot drift from the API and
cannot quietly stop doing anything: an example has to print something a reader
can recognise as the thing working. Add new ones to `scripts/corpus-gate.sh`.

Match a nested `Result` with one `match` per layer, the inner one inside the
outer `.Ok` arm — that is the shape every example here uses, and it is worth
more than the terser flattened pattern precisely because it is the same
everywhere.

## Commits

Conventional Commits: `<type>(<scope>): <subject>`, imperative and under 72
characters. Describe the outcome, not the files touched.
