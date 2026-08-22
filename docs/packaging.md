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

`lib` is the library `Cargo.toml` builds, so the two manifests have to agree;
`verify_package_contract.py` compares them rather than leaving the mismatch to
surface as a link error in someone else's build. Every package spells `crate`
as `"."`: one crate per package, in one place, never a second manifest for the
same sources.

The crate is a member of the workspace `Cargo.toml`, inherits its version from
`[workspace.package]`, exposes a C ABI over `hew-cabi` pinned at the same
revision as the compiler (see [`toolchain.md`](toolchain.md)), and owns its
resources behind opaque integer handles rather than raw pointers. Closing a
handle invalidates it: a later call against a closed handle returns a typed
error, it does not use freed memory.

## Package workflow

```sh
hew add hew.math.stats   # add a dependency
hew install              # resolve and fetch
hew publish              # publish this package
```

Within a package directory, `hew build` builds a declared `[native]` crate,
`hew check` validates a manifest or a source file, and `hew test` runs Hew
tests.

## Contract checks

`scripts/verify_package_contract.py` is the authority for the dotted-name
grammar, for the metadata every manifest must carry, and for the agreement
between a package's `hew.toml` and its Cargo crate — same version, same library
name, same crate type. It runs from `make verify` and again as the first action
of `scripts/publish-packages.sh`, so a local publish and a CI publish ask the
same question. It also holds the two counts this repository asserts about its
own tree — how many package manifests and how many `.hew` sources are tracked —
and hands the source count to `scripts/corpus-gate.sh`, so adding a package
means moving those numbers in one file.
