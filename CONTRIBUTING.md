# Contributing

## Before you push

```sh
make lint                                         # cargo fmt, clippy, shellcheck
docker compose up --detach --wait                 # the services the suites and examples use
scripts/corpus-gate.sh all                        # hew fmt, check, every suite and example
cargo test --locked --workspace --all-features    # the native crates
```

[`docs/toolchain.md`](docs/toolchain.md) describes the corpus gate's stages and
the system dependencies each one needs.

Everything must be warning-free under the pinned compiler, not just
error-free. A `lefthook` pre-commit hook formats staged files for you if you
have it installed.

## Adding a package

1. Create the directory matching the dotted name (`hew.queue.kafka` →
   `queue/kafka`) with a `hew.toml`, a module named after the final segment
   (`kafka.hew`), a `README.md`, `examples/`, and `tests/`.
2. Follow the [layout rules](docs/packaging.md#layout).
3. If the package wraps a Rust library, add the crate to the workspace
   `Cargo.toml` and follow the native-crate contract in
   [`docs/packaging.md`](docs/packaging.md).

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
