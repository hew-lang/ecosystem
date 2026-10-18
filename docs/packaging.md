# Packaging

## Layout

A package lives at the directory matching its dotted name: `hew.db.postgres`
is `db/postgres`, `hew.metrics` is `metrics`. Each package directory holds

```
hew.toml          package manifest (dotted name, version, compatibility range)
<segment>.hew     the module, named after the final path segment
README.md         one runnable example and the public surface
examples/         runnable programs, checked by CI like any other source
tests/            Hew test suites
src/, Cargo.toml  the native crate, when the package has one
```

`hew/` mirrors each top-level segment back with a symlink (`hew/db -> ../db`),
which is what lets `import hew.db.postgres` resolve inside this repository
without installing anything. **A new top-level segment needs a matching symlink
under `hew/`**, or every consumer of the new package fails to resolve it.

## Native crates

A package that wraps a Rust library keeps the crate in the package directory
itself — `src/lib.rs` beside `Cargo.toml` — and names it in `hew.toml`:

```toml
[native]
crate = "."
lib = "hew_hew_db_postgres"
kind = "staticlib"
```

`lib` is the library `Cargo.toml` builds, and `kind` is one of its crate types,
so the two manifests have to agree. Every package spells `crate` as `"."`: one
crate per package, in one place, never a second manifest for the same sources.

The crate is a member of the workspace `Cargo.toml`, inherits its version from
`[workspace.package]`, exposes a C ABI over `hew-cabi` pinned at the same
revision as the compiler (see [`toolchain.md`](toolchain.md)), and owns its
resources behind opaque integer handles rather than raw pointers. Closing a
handle invalidates it: a later call against a closed handle returns a typed
error, it does not use freed memory.

## Package workflow

The [getting-started example](../README.md#two-minutes-of-hew) shows both a
clone and a registry-backed project.

```sh
hew add hew.math.stats    # add a published package
hew install               # resolve and fetch
hew publish               # publish this package after installing its dependencies
```

Within a package directory, `hew build` builds a declared `[native]` crate,
`hew check` validates a manifest or a source file, and `hew test` runs Hew
tests.
