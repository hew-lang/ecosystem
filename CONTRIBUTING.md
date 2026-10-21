# Contributing

## Before you push

```sh
make lint                                         # cargo fmt and clippy
make check                                        # hew fmt and hew check, every source
docker compose up --detach --wait                 # the services the tests and examples use
make test test-native                             # every Hew test and example
make test-crates                                  # the native crates' Rust tests
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
in a sitting. Keep the public surface narrow, fail with a package-specific
error enum rather than sentinel values, give that enum an `error_message`, and
put the failure in the type rather than in a comment. Own external resources in
an actor so lifetime is explicit. Match variants in dotted form (`.Ok`, `.Err`).

Each README opens with one runnable example, and `examples/` holds the same
program as a file. An example has to print something a reader can recognise as
the thing working, and it carries a `#[test]` so `make test` runs it: one that
calls `main()` when the example asserts its own result, or one that checks the
printed value when the output is the point. Tests in `tests/` are `#[test]`
functions too, marked `#[real_time]` when they reach the package's native
code.

A fallible `receive fn` declares its failure with `-> T fails E`, not
`-> Result<T, E>`, so a caller gets one `Result<T, ActorError<E>>` rather than a
`Result` inside a `Result`. Match it in this order — the same shape in every
example here:

```hew
match conn.del(key) {
    .Ok(removed) => println(f"removed {removed}"),
    .Err(.Failed(error)) => println(redis.error_message(error)),
    .Err(_) => println("the actor stopped before replying"),
}
```

A test that expects the handler's error matches `.Err(.Failed(error))`, never
`.Err(_)`, which a stopped actor would also satisfy.

## Commits

Conventional Commits: `<type>(<scope>): <subject>`, imperative and under 72
characters. Describe the outcome, not the files touched.
