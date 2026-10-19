# Contributing

## Before you push

```sh
make lint                                         # cargo fmt and clippy
make check                                        # hew fmt and hew check, every source
docker compose up --detach --wait                 # the services the tests and examples use
make test test-native                             # every Hew test and example
cargo test --locked --workspace --all-features    # the native crates
```

[`docs/toolchain.md`](docs/toolchain.md) describes these targets and the system
dependencies each one needs.

Everything must be warning-free under the pinned compiler, not just
error-free. A `lefthook` pre-commit hook formats staged files for you if you
have it installed.

## Adding a package

1. Create the directory matching the dotted name (`hew.queue.kafka` →
   `queue/kafka`) with a `hew.toml`, a module named after the final segment
   (`kafka.hew`), a `README.md`, `examples/`, and `tests/`, and list the
   directory in the root `hew.toml` workspace members. `make test` finds the
   new package by its `hew.toml`.
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
program as a file. An example has to print something a reader can recognise as
the thing working, and it carries a `#[test]` so `make test` runs it: one that
calls `main()` when the example asserts its own result, or one that checks the
printed value when the output is the point. Tests in `tests/` are `#[test]`
functions too, marked `#[real_time]` when they reach the package's native
code.

Match a nested `Result` with one `match` per layer, the inner one inside the
outer `.Ok` arm — that is the shape every example here uses, and it is worth
more than the terser flattened pattern precisely because it is the same
everywhere.

## Commits

Conventional Commits: `<type>(<scope>): <subject>`, imperative and under 72
characters. Describe the outcome, not the files touched.
